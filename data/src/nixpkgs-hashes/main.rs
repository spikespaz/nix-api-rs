use std::ffi::OsString;
use std::process::Stdio;

use include_dir::{Dir, include_dir};
use smol::io::{AsyncBufReadExt, BufReader};
use smol::process::Command;
use smol::stream::StreamExt;
use sonic_rs::JsonValueTrait;
use tempfile::TempDir;

static NPINS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/nixpkgs-hashes/npins");
static JOBS_EXPR: &str = include_str!("nixpkgs-release.nix");

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

    smol::block_on(async {
        let mut eval_drvs = Command::new("nix-eval-jobs")
            .arg("--workers")
            .arg(std::thread::available_parallelism()?.to_string())
            .arg("--force-recurse")
            .arg("--expr")
            .arg(OsString::from_iter([
                "import ".as_ref(),
                expr_dir.path().canonicalize()?.as_ref(),
            ]))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;

        let eval_stdout = eval_drvs.stdout.take().unwrap();
        let mut json_lines = BufReader::new(eval_stdout).lines();

        while let Some(json_line) = json_lines.try_next().await? {
            let Ok(drv_path) = sonic_rs::get_from_str(&json_line, ["drvPath"]) else {
                assert!(sonic_rs::get_from_str(&json_line, ["error"]).is_ok());
                continue;
            };
            let drv_path = drv_path.as_str().unwrap();
            eprintln!("drv_path = {drv_path}")
        }

        let status = eval_drvs.status().await?;
        if !status.success() {
            panic!("nix-eval-jobs exited with {status}");
        }

        Ok::<_, std::io::Error>(())
    })?;

    expr_dir.close()?;
    Ok(())
}
