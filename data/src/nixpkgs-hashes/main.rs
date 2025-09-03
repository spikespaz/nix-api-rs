use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use humantime::{FormattedDuration, format_duration};
use include_dir::{Dir, include_dir};
use smol::fs::File;
use smol::future::try_zip;
use smol::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use smol::lock::Semaphore;
use smol::process::Command;
use smol::stream::{Stream, StreamExt, try_unfold};
use smol::{LocalExecutor, channel};
use sonic_rs::{JsonValueTrait, LazyValue, PointerTree};
use tempfile::TempDir;

static NPINS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/nixpkgs-hashes/npins");
static JOBS_EXPR: &str = include_str!("nixpkgs-release.nix");

static GENERATE_OUTPUT_FILE_NAME: &str = "nixpkgs-hashes.csv";
const STORE_PATHS_PER_QUERY: usize = 8;
const MAX_CONCURRENT_STORE_QUERIES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Hash {
    pub hash: String,
    pub algo: Option<String>,
}

struct DerivationHashes {
    pub env: Option<Hash>,
    pub outputs: Vec<(String, Hash)>,
}

enum Statistic {
    Progress {
        drvs: usize,
        hashes: usize,
        total_unique: usize,
    },
}

struct TimingBucket<const SCALE: u64> {
    last_total: u64,
    last_update: Instant,
    since_start: Duration,
    since_mark: Duration,
}

fn main() -> std::io::Result<()> {
    let expr_dir = {
        let dir = TempDir::with_prefix("nixpkgs-release")?;
        let npins_path = dir.path().join("npins");
        let expr_path = dir.path().join("default.nix");
        std::fs::create_dir(&npins_path)?;
        NPINS_DIR.extract(&npins_path)?;
        std::fs::write(&expr_path, JOBS_EXPR)?;
        dir
    };
    let expr_path = expr_dir.path().canonicalize()?;

    println!("STORE_PATHS_PER_QUERY = {STORE_PATHS_PER_QUERY}");
    println!("MAX_CONCURRENT_STORE_QUERIES = {MAX_CONCURRENT_STORE_QUERIES}");

    let ex = LocalExecutor::new();
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_STORE_QUERIES));
    let (chunks_tx, chunks_rx) = channel::unbounded();
    let (stats_tx, stats_rx) = channel::bounded(1);

    let dispatcher = async {
        let drvs_expr = OsString::from_iter(["import ".as_ref(), expr_path.as_ref()]);
        let eval_drvs = nix_eval_jobs(true, drvs_expr).await?;
        smol::pin!(eval_drvs);

        loop {
            let mut chunk = (&mut eval_drvs).take(STORE_PATHS_PER_QUERY);
            let mut batch = Vec::with_capacity(STORE_PATHS_PER_QUERY);
            while let Some(drv_path) = chunk.try_next().await? {
                batch.push(drv_path);
            }
            if batch.is_empty() {
                break;
            }
            let permit = sem.acquire_arc().await;
            let tx = chunks_tx.clone();
            ex.spawn(async move {
                let hashes = collect_hashes_for_many_derivations(batch).await;
                tx.send(hashes).await.unwrap();
                drop(permit);
            })
            .detach();
        }

        drop(chunks_tx);
        Ok::<_, std::io::Error>(())
    };

    let receiver = async {
        let output_file = File::create(GENERATE_OUTPUT_FILE_NAME).await?;
        let mut writer = BufWriter::new(output_file);
        let mut unique = HashSet::new();

        let mut write_unique_hash = async |unique: &mut HashSet<_>, hash: &Hash| {
            if unique.insert(hash.clone()) {
                let csv_record = hash.to_csv_record().to_string();
                writer.write_all(csv_record.as_bytes()).await?;
                writer.write_all(b"\n").await?;
            }
            Ok::<_, std::io::Error>(())
        };

        while let Ok(res) = chunks_rx.recv().await {
            let drv_hashes = res?;
            let mut hash_count = 0;
            let drv_count = drv_hashes.len();

            for (_drv_path, DerivationHashes { env, outputs }) in drv_hashes {
                if let Some(env_hash) = env {
                    write_unique_hash(&mut unique, &env_hash).await?;
                    hash_count += 1;
                }
                for (_out_name, out_hash) in outputs {
                    write_unique_hash(&mut unique, &out_hash).await?;
                    hash_count += 1;
                }
            }

            stats_tx
                .send(Statistic::Progress {
                    drvs: drv_count,
                    hashes: hash_count,
                    total_unique: unique.len(),
                })
                .await
                .unwrap();
        }

        writer.close().await?;
        Ok::<_, std::io::Error>(unique)
    };

    let statistics = async move {
        let mut total_drvs = 0;
        let mut total_hashes = 0;
        let start = Instant::now();

        let mut time_1k = TimingBucket::<1_000>::new(start);
        let mut time_10k = TimingBucket::<10_000>::new(start);
        let mut time_100k = TimingBucket::<100_000>::new(start);

        while let Ok(msg) = stats_rx.recv().await {
            match msg {
                Statistic::Progress {
                    drvs,
                    hashes,
                    total_unique,
                } => {
                    total_hashes += hashes as u64;
                    total_drvs += drvs as u64;
                    let now = Instant::now();

                    time_1k.update(now, total_hashes);
                    time_10k.update(now, total_hashes);
                    time_100k.update(now, total_hashes);

                    println!(
                        "[progress] drvs: {total_drvs}, hashes: {total_hashes} (unique: {total_unique}), elapsed: {}",
                        DisplayElapsed::from(now - start),
                    );
                    println!(
                        "[perf (s/hash)] {time_1k:>width_0$}, {time_10k:>width_1$}, {time_100k:>#width_2$}",
                        width_0 = 9,
                        width_1 = 10,
                        width_2 = 12,
                    );
                }
            }
        }
    };

    let _hashes = smol::block_on(ex.run(async {
        let statistics_ = ex.spawn(statistics);
        let (_, hashes) = try_zip(dispatcher, receiver).await?;
        statistics_.await;
        Ok::<_, std::io::Error>(hashes)
    }))?;

    expr_dir.close()?;
    Ok(())
}

impl Hash {
    fn to_csv_record(&self) -> impl std::fmt::Display {
        struct __Display<'a>(&'a Hash);
        impl<'a> std::fmt::Display for __Display<'a> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, r#""{}""#, self.0.hash)?;
                write!(f, ", ")?;
                match &self.0.algo {
                    Some(algo) => write!(f, r#""{algo}""#)?,
                    None => write!(f, "null")?,
                }
                Ok(())
            }
        }
        __Display(self)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExitStatusError(ExitStatus);

impl std::error::Error for ExitStatusError {}

impl std::fmt::Display for ExitStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.0.code() {
            write!(f, "exited with code: {code}")
        } else if let Some(signal) = self.0.signal() {
            write!(f, "killed by signal: {signal}")
        } else {
            write!(f, "exited with status: {}", self.0)
        }
    }
}

async fn nix_eval_jobs(
    force_recurse: bool,
    expr: impl AsRef<OsStr>,
) -> std::io::Result<impl Stream<Item = std::io::Result<String>>> {
    let mut cmd = Command::new("nix-eval-jobs");
    if force_recurse {
        cmd.arg("--force-recurse");
    }
    cmd.arg("--expr").arg(expr.as_ref());
    cmd.arg("--workers")
        .arg(std::thread::available_parallelism()?.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut proc = cmd.spawn()?;
    let stdout = proc.stdout.take().unwrap();
    let drv_paths = BufReader::new(stdout).lines().filter_map(|res| {
        if let Ok(line) = res {
            if let Ok(drv_path) = sonic_rs::get_from_str(&line, ["drvPath"]) {
                let drv_path = drv_path.as_str().unwrap().to_string();
                Some(Ok(drv_path))
            } else {
                assert!(sonic_rs::get_from_str(&line, ["error"]).is_ok());
                None
            }
        } else {
            Some(res)
        }
    });

    let check_status = |status: ExitStatus| {
        if status.success() {
            Ok(())
        } else {
            use std::io::Error;
            Err(Error::other(ExitStatusError(status)))
        }
    };
    let stream = try_unfold(
        (proc, drv_paths),
        move |(mut proc, mut drv_paths)| async move {
            if let Some(status) = proc.try_status()? {
                check_status(status).map(|_| None)
            } else if let Some(drv_path) = drv_paths.try_next().await? {
                Ok(Some((drv_path, (proc, drv_paths))))
            } else {
                let status = proc.status().await?;
                check_status(status).map(|_| None)
            }
        },
    );

    Ok(stream)
}

async fn collect_hashes_for_many_derivations(
    drvs: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> std::io::Result<Vec<(String, DerivationHashes)>> {
    let output = Command::new("nix")
        .args(["derivation", "show", "--recursive"])
        .args(drvs)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        todo!()
    }
    let drv_hashes = sonic_rs::to_object_iter(output.stdout.as_slice()).map(|res| {
        let (drv_path, drv_json) = res.unwrap();
        (drv_path.to_string(), hashes_for_derivation(&drv_json))
    });
    Ok(drv_hashes.collect())
}

fn hashes_for_derivation(json: &LazyValue) -> DerivationHashes {
    static PATHS: LazyLock<PointerTree> = LazyLock::new(|| {
        let mut paths = PointerTree::new();
        paths.add_path(&["env", "outputHash"]);
        paths.add_path(&["env", "outputHashAlgo"]);
        paths.add_path(&["outputs"]);
        paths
    });
    let values = sonic_rs::get_many(json.as_raw_str(), &PATHS).unwrap();
    let [env_hash, env_hash_algo, outputs] = values.try_into().unwrap();

    let env_hash = env_hash.as_ref().map(|v| v.as_str().unwrap());
    let env_hash_algo = env_hash_algo.as_ref().and_then(|v| match v {
        v if v.is_null() => None,
        v => Some(v.as_str().unwrap()),
    });
    let outputs = outputs.and_then(LazyValue::into_object_iter).unwrap();

    let env = env_hash.map(|hash| Hash {
        hash: hash.to_string(),
        algo: env_hash_algo.map(str::to_string),
    });

    let outputs = outputs
        .map(Result::unwrap)
        .filter_map(|(out_name, out_json)| {
            static PATHS: LazyLock<PointerTree> = LazyLock::new(|| {
                let mut paths = PointerTree::new();
                paths.add_path(&["hash"]);
                paths.add_path(&["hashAlgo"]);
                paths
            });
            let values = sonic_rs::get_many(out_json.as_raw_str(), &PATHS).unwrap();
            let [hash, algo] = values.try_into().unwrap();

            let hash = hash.map(|v| v.as_str().unwrap().to_string())?;
            let algo = algo.map(|v| v.as_str().unwrap().to_string()).unwrap();

            Some((out_name.to_string(), Hash::with_algo(hash, algo)))
        })
        .collect();

    DerivationHashes { env, outputs }
}

impl Hash {
    fn with_algo(hash: impl Into<String>, algo: impl Into<String>) -> Self {
        Self {
            hash: hash.into(),
            algo: Some(algo.into()),
        }
    }
}

impl<const SCALE: u64> TimingBucket<SCALE> {
    fn new(start: Instant) -> Self {
        debug_assert!(SCALE > 0 && SCALE % 1000 == 0);
        Self {
            last_total: 0,
            last_update: start,
            since_start: Duration::ZERO,
            since_mark: Duration::ZERO,
        }
    }

    pub fn marks_passed(&self) -> u64 {
        self.last_total / SCALE
    }

    pub fn update(&mut self, now: Instant, curr_total: u64) {
        assert!(curr_total >= self.last_total);
        assert!(now >= self.last_update);

        let delta = curr_total - self.last_total;
        let elapsed = now - self.last_update;
        self.last_update = now;
        self.since_start += elapsed;

        let last_mark = self.marks_passed();
        self.last_total = curr_total;
        let curr_mark = self.marks_passed();

        if curr_mark > last_mark {
            let progress = curr_total % SCALE;
            let complete = delta - progress;

            let elapsed_ns = elapsed.as_nanos();
            let attributed = elapsed_ns * (complete as u128) / (delta as u128);
            let since_mark = elapsed_ns - attributed;

            self.since_mark = Duration::from_nanos(since_mark as u64);
        } else {
            self.since_mark += elapsed;
        }
    }

    pub fn average_rate(&self) -> Option<Duration> {
        let marks = self.marks_passed();
        if marks == 0 {
            None
        } else {
            let time_to_mark = self.since_start - self.since_mark;
            Some(time_to_mark / marks as u32)
        }
    }

    pub fn average_rate_predictive(&self) -> Option<Duration> {
        if self.last_total == 0 {
            None
        } else {
            let elapsed = self.since_start.as_nanos();
            let rate = elapsed * SCALE as u128 / self.last_total as u128;
            Some(Duration::from_nanos(rate as u64))
        }
    }
}

impl<const SCALE: u64> std::fmt::Display for TimingBucket<SCALE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let thou = SCALE / 1000;
        let rate = if f.alternate() {
            self.average_rate_predictive().map(|d| d.as_secs_f64())
        } else {
            self.average_rate().map(|d| d.as_secs_f64())
        };
        let precision = f.precision().unwrap_or(2);
        match (f.width(), rate) {
            (Some(_), None) => f.pad(&format!("--/{thou}k")),
            (Some(_), Some(rate)) => f.pad(&format!("{rate:.precision$}s/{thou}k")),
            (None, None) => write!(f, "--/{thou}k"),
            (None, Some(rate)) => write!(f, "{rate:.precision$}s/{thou}k"),
        }
    }
}

struct DisplayElapsed(FormattedDuration);

impl From<Duration> for DisplayElapsed {
    fn from(other: Duration) -> Self {
        Self(format_duration({
            if other >= Duration::from_secs(60) {
                Duration::from_secs(other.as_secs())
            } else {
                Duration::from_millis(other.as_millis() as u64)
            }
        }))
    }
}

impl std::fmt::Display for DisplayElapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
