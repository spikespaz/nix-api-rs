use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::process::{ExitStatus, Stdio};
use std::sync::LazyLock;

use include_dir::{Dir, include_dir};
use smol::io::{AsyncBufReadExt, BufReader};
use smol::process::Command;
use smol::stream::{Stream, StreamExt, try_unfold};
use sonic_rs::{JsonValueTrait, LazyValue, PointerTree};
use tempfile::TempDir;

static NPINS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/nixpkgs-hashes/npins");
static JOBS_EXPR: &str = include_str!("nixpkgs-release.nix");

#[derive(Debug, PartialEq, Eq, Hash)]
struct Hash {
    pub hash: String,
    pub algo: Option<String>,
}

struct DerivationHashes {
    pub env: Option<Hash>,
    pub outputs: Vec<(String, Hash)>,
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

    smol::block_on(async {
        let drvs_expr = OsString::from_iter(["import ".as_ref(), expr_path.as_ref()]);
        let eval_drvs = nix_eval_jobs(true, drvs_expr).await?;
        smol::pin!(eval_drvs);

        let mut unique_hashes = HashSet::new();

        while let Some(drv_path) = eval_drvs.try_next().await? {
            let drv_hashes = collect_hashes_for_many_derivations(&[drv_path]).await?;
            for (drv_path, DerivationHashes { env, outputs }) in drv_hashes {
                if let Some(env_hash) = env {
                    eprintln!("{drv_path} = {env_hash:?}");
                    unique_hashes.insert(env_hash);
                }
                for (out_name, out_hash) in outputs {
                    eprintln!("{drv_path}/{out_name} = {out_hash:?}");
                    unique_hashes.insert(out_hash);
                }
            }
        }

        Ok::<_, std::io::Error>(())
    })?;

    expr_dir.close()?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExitStatusError(ExitStatus);

impl std::error::Error for ExitStatusError {}

impl std::fmt::Display for ExitStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exited with status: {}", self.0)
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

    let stream = try_unfold((proc, drv_paths), |(proc, mut drv_paths)| async {
        if let Some(drv_path) = drv_paths.try_next().await? {
            Ok(Some((drv_path, (proc, drv_paths))))
        } else {
            let mut proc = proc;
            proc.status().await.and_then(|status| {
                if status.success() {
                    Ok(None)
                } else {
                    use std::io::{Error, ErrorKind};
                    Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        ExitStatusError(status),
                    ))
                }
            })
        }
    });

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
