use config::Config;
use crates::{Crate, RegistryCrate};
use dirs::{self, EXPERIMENT_DIR, TEST_SOURCE_DIR};
use errors::*;
use file;
use git;
use lists::{self, List};
use results::WriteResults;
use run::RunCommand;
use serde_json;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use toml_frobber;
use toolchain::{self, CargoState, Toolchain};
use util;

string_enum!(pub enum ExMode {
    BuildAndTest => "build-and-test",
    BuildOnly => "build-only",
    CheckOnly => "check-only",
    UnstableFeatures => "unstable-features",
});

string_enum!(pub enum ExCrateSelect {
    Full => "full",
    Demo => "demo",
    SmallRandom => "small-random",
    Top100 => "top-100",
});

string_enum!(pub enum ExCapLints {
    Allow => "allow",
    Warn => "warn",
    Deny => "deny",
    Forbid => "forbid",
});

pub fn ex_dir(ex_name: &str) -> PathBuf {
    EXPERIMENT_DIR.join(ex_name)
}

pub fn config_file(ex_name: &str) -> PathBuf {
    EXPERIMENT_DIR.join(ex_name).join("config.json")
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Experiment {
    pub name: String,
    pub crates: Vec<Crate>,
    pub toolchains: Vec<Toolchain>,
    pub mode: ExMode,
    pub cap_lints: ExCapLints,
    pub rustflags: Option<String>,
}

pub struct ExOpts {
    pub name: String,
    pub toolchains: Vec<Toolchain>,
    pub mode: ExMode,
    pub crates: ExCrateSelect,
    pub cap_lints: ExCapLints,
    pub rustflags: Option<String>,
}

pub fn get_crates(crates: ExCrateSelect, config: &Config) -> Result<Vec<Crate>> {
    match crates {
        ExCrateSelect::Full => lists::read_all_lists(),
        ExCrateSelect::Demo => demo_list(config),
        ExCrateSelect::SmallRandom => small_random(),
        ExCrateSelect::Top100 => top_100(),
    }
}

pub fn define(opts: ExOpts, config: &Config) -> Result<()> {
    delete(&opts.name)?;
    define_(
        &opts.name,
        opts.toolchains,
        get_crates(opts.crates, config)?,
        opts.mode,
        opts.cap_lints,
        opts.rustflags,
    )
}

pub fn demo_list(config: &Config) -> Result<Vec<Crate>> {
    let mut crates = config.demo_crates().crates.iter().collect::<HashSet<_>>();
    let repos = &config.demo_crates().github_repos;
    let expected_len = crates.len() + repos.len();

    let result = lists::read_all_lists()?
        .into_iter()
        .filter(|c| match *c {
            Crate::Registry(RegistryCrate { ref name, .. }) => crates.remove(name),
            Crate::GitHub(ref repo) => {
                let url = repo.url();

                let mut found = false;
                for repo in repos {
                    if url.ends_with(repo) {
                        found = true;
                        break;
                    }
                }

                found
            }
        })
        .collect::<Vec<_>>();

    assert_eq!(result.len(), expected_len);
    Ok(result)
}

fn small_random() -> Result<Vec<Crate>> {
    use rand::{thread_rng, Rng};

    const COUNT: usize = 20;

    let mut crates = lists::read_all_lists()?;
    let mut rng = thread_rng();
    rng.shuffle(&mut crates);

    crates.truncate(COUNT);
    crates.sort();

    Ok(crates)
}

fn top_100() -> Result<Vec<Crate>> {
    let mut crates = lists::PopList::read()?;
    crates.truncate(100);
    Ok(crates)
}

pub fn define_(
    ex_name: &str,
    toolchains: Vec<Toolchain>,
    crates: Vec<Crate>,
    mode: ExMode,
    cap_lints: ExCapLints,
    rustflags: Option<String>,
) -> Result<()> {
    info!(
        "defining experiment {} for {} crates",
        ex_name,
        crates.len()
    );
    let ex = Experiment {
        name: ex_name.to_string(),
        crates,
        toolchains,
        mode,
        cap_lints,
        rustflags,
    };

    ex.validate()?;

    fs::create_dir_all(&ex_dir(&ex.name))?;
    let json = serde_json::to_string(&ex)?;
    info!("writing ex config to {}", config_file(ex_name).display());
    file::write_string(&config_file(ex_name), &json)?;
    Ok(())
}

impl Experiment {
    pub fn validate(&self) -> Result<()> {
        if self.toolchains[0] == self.toolchains[1] {
            bail!("reusing the same toolchain isn't supported");
        }

        if self.rustflags.is_some()
            && !self.toolchains[0].enable_rustflags
            && !self.toolchains[1].enable_rustflags
        {
            bail!("rustflags are present but no toolchain is using them");
        }

        if self.rustflags.is_none()
            && (self.toolchains[0].enable_rustflags || self.toolchains[1].enable_rustflags)
        {
            bail!("a toolchain is enabling rustflags but none are set");
        }

        Ok(())
    }

    pub fn fetch_repo_crates(&self) -> Result<()> {
        for repo in self.crates.iter().filter_map(|krate| krate.github()) {
            if let Err(e) = git::shallow_clone_or_pull(&repo.url(), &repo.mirror_dir()) {
                util::report_error(&e);
            }
        }
        Ok(())
    }
}

impl Experiment {
    pub fn load(ex_name: &str) -> Result<Self> {
        let config = file::read_string(&config_file(ex_name))?;
        Ok(serde_json::from_str(&config)?)
    }
}

#[cfg_attr(feature = "cargo-clippy", allow(match_ref_pats))]
pub fn frob_toml(ex: &Experiment, tc: &Toolchain, krate: &Crate) -> Result<()> {
    if let Crate::Registry(_) = *krate {
        toml_frobber::frob_toml(&dirs::ex_crate_source(ex, tc, krate), krate)?;
    }

    Ok(())
}

pub fn capture_shas<DB: WriteResults>(ex: &Experiment, crates: &[Crate], db: &DB) -> Result<()> {
    for krate in crates {
        if let Crate::GitHub(ref repo) = *krate {
            let dir = repo.mirror_dir();
            let r = RunCommand::new("git", &["rev-parse", "HEAD"])
                .cd(&dir)
                .run_capture();

            let sha = match r {
                Ok((stdout, _)) => if let Some(shaline) = stdout.get(0) {
                    if !shaline.is_empty() {
                        info!("sha for GitHub repo {}: {}", repo.slug(), shaline);
                        shaline.to_string()
                    } else {
                        bail!("bogus output from git log for {}", dir.display());
                    }
                } else {
                    bail!("bogus output from git log for {}", dir.display());
                },
                Err(e) => {
                    bail!("unable to capture sha for {}: {}", dir.display(), e);
                }
            };

            db.record_sha(ex, repo, &sha)
                .chain_err(|| format!("failed to record the sha of GitHub repo {}", repo.slug()))?;
        }
    }

    Ok(())
}

fn crate_work_dir(ex: &Experiment, toolchain: &Toolchain, krate: &Crate) -> PathBuf {
    TEST_SOURCE_DIR
        .join(&ex.name)
        .join(toolchain.to_string())
        .join(krate.id())
}

pub fn with_work_crate<F, R>(
    ex: &Experiment,
    toolchain: &Toolchain,
    krate: &Crate,
    allow_source_changes: bool,
    f: F,
) -> Result<R>
where
    F: Fn(&Path) -> Result<R>,
{
    let src_dir = dirs::ex_crate_source(ex, toolchain, krate);

    if allow_source_changes {
        f(&src_dir)
    } else {
        let dest_dir = crate_work_dir(ex, toolchain, krate);
        info!(
            "creating temporary build dir for {} in {}",
            krate,
            dest_dir.display()
        );

        util::copy_dir(&src_dir, &dest_dir)?;
        let r = f(&dest_dir);
        util::remove_dir_all(&dest_dir)?;
        r
    }
}

pub fn capture_lockfile(
    config: &Config,
    ex: &Experiment,
    toolchain: &Toolchain,
    krate: &Crate,
) -> Result<()> {
    let lockfile = dirs::ex_crate_source(ex, toolchain, krate).join("Cargo.lock");
    if !config.should_update_lockfile(krate) && lockfile.exists() {
        info!("crate {} has a lockfile. skipping", krate);
        return Ok(());
    }

    with_work_crate(ex, toolchain, krate, true, |path| {
        let args = &[
            "generate-lockfile",
            "--manifest-path",
            "Cargo.toml",
            "-Zno-index-update",
        ];
        toolchain
            .run_cargo(ex, path, args, CargoState::Unlocked, false, false)
            .chain_err(|| format!("unable to generate lockfile for {}", krate))?;

        info!("generated lockfile for {}", krate);
        Ok(())
    }).chain_err(|| format!("failed to generate lockfile for {}", krate))?;

    Ok(())
}

pub fn fetch_crate_deps(ex: &Experiment, toolchain: &Toolchain, krate: &Crate) -> Result<()> {
    with_work_crate(ex, toolchain, krate, false, |path| {
        let args = &["fetch", "--locked", "--manifest-path", "Cargo.toml"];
        toolchain
            .run_cargo(ex, path, args, CargoState::Unlocked, false, true)
            .chain_err(|| format!("unable to fetch deps for {}", krate))?;

        Ok(())
    })
}

pub fn prepare_all_toolchains(ex: &Experiment) -> Result<()> {
    for tc in &ex.toolchains {
        tc.prepare()?;
    }

    Ok(())
}

pub fn copy(ex1_name: &str, ex2_name: &str) -> Result<()> {
    let ex1_dir = &ex_dir(ex1_name);
    let ex2_dir = &ex_dir(ex2_name);

    if !ex1_dir.exists() {
        bail!("experiment {} is not defined", ex1_name);
    }

    if ex2_dir.exists() {
        bail!("experiment {} is already defined", ex2_name);
    }

    util::copy_dir(ex1_dir, ex2_dir)
}

pub fn delete_all_target_dirs(ex_name: &str) -> Result<()> {
    let target_dir = &toolchain::ex_target_dir(ex_name);
    if target_dir.exists() {
        util::remove_dir_all(target_dir)?;
    }

    Ok(())
}

pub fn delete(ex_name: &str) -> Result<()> {
    let ex_dir = ex_dir(ex_name);
    if ex_dir.exists() {
        util::remove_dir_all(&ex_dir)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ExCapLints, ExMode, Experiment};

    #[test]
    fn test_validate_experiment() {
        // Correct experiment
        assert!(
            Experiment {
                name: "foo".to_string(),
                crates: vec![],
                toolchains: vec!["stable".parse().unwrap(), "beta".parse().unwrap()],
                mode: ExMode::BuildAndTest,
                cap_lints: ExCapLints::Forbid,
                rustflags: None,
            }.validate()
                .is_ok()
        );

        // Experiment with the same toolchain
        assert!(
            Experiment {
                name: "foo".to_string(),
                crates: vec![],
                toolchains: vec!["stable".parse().unwrap(), "stable".parse().unwrap()],
                mode: ExMode::BuildAndTest,
                cap_lints: ExCapLints::Forbid,
                rustflags: None,
            }.validate()
                .is_err()
        );

        // Experiment with rustflags but no toolchain using them
        assert!(
            Experiment {
                name: "foo".to_string(),
                crates: vec![],
                toolchains: vec!["stable".parse().unwrap(), "beta".parse().unwrap()],
                mode: ExMode::BuildAndTest,
                cap_lints: ExCapLints::Forbid,
                rustflags: Some("-Zfoo".into()),
            }.validate()
                .is_err()
        );

        // Experiment with no rustflags but a toolchain using them
        assert!(
            Experiment {
                name: "foo".to_string(),
                crates: vec![],
                toolchains: vec!["stable".parse().unwrap(), "beta+rustflags".parse().unwrap()],
                mode: ExMode::BuildAndTest,
                cap_lints: ExCapLints::Forbid,
                rustflags: None,
            }.validate()
                .is_err()
        );
    }
}
