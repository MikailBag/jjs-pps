use crate::{
    apis::compile::{
        build::{BuildBackend, Task, TaskError},
        CompileUpdate,
    },
    command::Command,
    operation::ProgressWriter,
};
use anyhow::Context as _;
use pom::{FileRef, FileRefRoot, Limits};
use std::{
    collections::HashMap,
    fmt::Write,
    os::unix::io::IntoRawFd,
    path::{Path, PathBuf},
    process::Stdio,
};

/// ProblemBuilder is struct, responsible for building single problem.
/// Its instances are managed by CompilerService.
pub(crate) struct ProblemBuilder<'a> {
    /// Problem manifest
    pub(crate) cfg: &'a crate::manifest::Problem,
    /// Directory, containing problem source files
    pub(crate) problem_dir: &'a Path,
    /// Directory for output files
    pub(crate) out_dir: &'a Path,
    /// Path to problem build environment
    pub(crate) build_env: &'a Path,
    /// Used to execute build tasks (e.g. builds checker or solution)
    pub(crate) build_backend: &'a dyn BuildBackend,
    /// Used to return live building progress
    pub(crate) pw: &'a mut ProgressWriter<CompileUpdate>,
}

/// Fills given buffer with random hex string
fn get_entropy_hex(buf: &mut [u8]) {
    getrandom::getrandom(buf).expect("get entropy failed");
    for i in buf.iter_mut() {
        *i %= 16;
        if *i < 10 {
            *i += b'0';
        } else {
            *i = b'a' + (*i - 10);
        }
    }
}

/// Applies merge patch `other` to a `place`:
/// If `other` is None, does nothing.
/// If `other` is Some, stores `other` inner value into `place`.
fn merge_option<T: Copy>(place: &mut Option<T>, other: Option<T>) {
    if let Some(x) = other {
        place.replace(x);
    }
}

/// Merges several `Limits` object. Last element of slice will have maximal proirity.
fn merge_limits(limits_set: &[Limits]) -> Limits {
    let mut res = Limits::default();
    for lim in limits_set {
        merge_option(&mut res.memory, lim.memory);
        merge_option(&mut res.process_count, lim.process_count);
        merge_option(&mut res.time, lim.time);
    }
    res
}

// TODO: remove duplicated code
impl<'a> ProblemBuilder<'a> {
    /// Higher-level wrapper for `self.build_backend`
    async fn do_build(&self, src: &Path, dest: &Path) -> anyhow::Result<Command> {
        tokio::fs::create_dir_all(dest)
            .await
            .context("failed to create dir")?;

        let build_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros()
            .to_string();
        let build_dir = format!("/tmp/pps-build-{}", &build_id);
        tokio::fs::create_dir(&build_dir)
            .await
            .expect("couldn't create build dir");

        let task = Task {
            src: src.to_path_buf(),
            dest: dest.to_path_buf(),
            tmp: Path::new(&build_dir).to_path_buf(),
        };
        match self.build_backend.process_task(task.clone()).await {
            Ok(cmd) => Ok(cmd.command),
            Err(err) => {
                let mut description = String::new();
                writeln!(
                    &mut description,
                    "Build error: unable to run build task: {}",
                    err
                )
                .unwrap();
                if let TaskError::ExitCodeNonZero(cmd, out) = err {
                    writeln!(&mut description, "Command: {}", cmd).unwrap();
                    writeln!(
                        &mut description,
                        "--- stdout ---\n{}",
                        String::from_utf8_lossy(&out.stdout)
                    )
                    .unwrap();
                    writeln!(
                        &mut description,
                        "--- stderr ---\n{}",
                        String::from_utf8_lossy(&out.stderr)
                    )
                    .unwrap();
                }
                writeln!(&mut description, "Build task: {:#?}", task).unwrap();
                anyhow::bail!("task execution error: {}", description)
            }
        }
    }

    /// async wrapper for `glob::glob`
    async fn glob(&self, suffix: &str) -> anyhow::Result<Vec<PathBuf>> {
        let pattern = format!("{}/{}", self.problem_dir.display(), suffix);
        tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<PathBuf>> {
            let paths = glob::glob(&pattern)
                .context("blob pattern error")?
                .map(|x| match x {
                    Ok(p) => Ok(p),
                    Err(err) => {
                        anyhow::bail!("Glob error: {}", err);
                    }
                })
                .collect::<anyhow::Result<Vec<PathBuf>>>()?;
            Ok(paths)
        })
        .await
        .unwrap()
    }

    /// Builds single solution
    async fn build_solution(&mut self, sol_path: PathBuf) -> anyhow::Result<(String, Command)> {
        let sol_id = sol_path
            .file_stem()
            .context("missing file stem on solution path")?
            .to_str()
            .context("path is not utf8")?
            .to_owned();
        self.pw
            .send(CompileUpdate::BuildSolution(sol_id.clone()))
            .await;

        let out_path = format!("{}/assets/sol-{}", self.out_dir.display(), &sol_id);
        Ok((
            sol_id,
            self.do_build(&sol_path, &PathBuf::from(&out_path)).await?,
        ))
    }

    /// Builds all solutions
    async fn build_solutions(&mut self) -> anyhow::Result<HashMap<String, Command>> {
        let mut out = HashMap::new();
        for solution_path in self.glob("solutions/*").await? {
            let (sol_id, cmd) = self.build_solution(solution_path).await?;
            out.insert(sol_id, cmd);
        }
        Ok(out)
    }

    /// Builds single testgen
    async fn build_testgen(
        &mut self,
        testgen_path: &Path,
        testgen_name: &str,
    ) -> anyhow::Result<Command> {
        self.pw
            .send(CompileUpdate::BuildTestgen(testgen_name.to_string()))
            .await;
        let out_path = format!("{}/assets/testgen-{}", self.out_dir.display(), testgen_name);
        self.do_build(testgen_path, &Path::new(&out_path)).await
    }

    /// Builds all testgens
    async fn build_testgens(&mut self) -> anyhow::Result<HashMap<String, Command>> {
        let mut out = HashMap::new();
        for testgen in self.glob("generators/*").await? {
            let testgen_name = testgen
                .file_stem()
                .unwrap()
                .to_str()
                .context("utf8 error")?;
            let testgen_launch_cmd = self.build_testgen(&testgen, testgen_name).await?;
            out.insert(testgen_name.to_string(), testgen_launch_cmd);
        }
        Ok(out)
    }

    /// Adds common modifications to a child process builder
    fn configure_command(&self, cmd: &mut Command) {
        cmd.current_dir(self.problem_dir);
        cmd.env("JJS_PROBLEM_SRC", &self.problem_dir);
        cmd.env("JJS_PROBLEM_DEST", &self.out_dir);
    }

    /// Builds all tests
    async fn build_tests(
        &mut self,
        testgens: &HashMap<String, Command>,
        gen_answers: Option<&Command>,
    ) -> anyhow::Result<Vec<pom::Test>> {
        let tests_path = format!("{}/assets/tests", self.out_dir.display());
        std::fs::create_dir_all(&tests_path).expect("couldn't create tests output dir");
        self.pw
            .send(CompileUpdate::GenerateTests {
                count: self.cfg.tests.len(),
            })
            .await;
        let mut out = vec![];
        for (i, test_spec) in self.cfg.tests.iter().enumerate() {
            let tid = i + 1;
            self.pw
                .send(CompileUpdate::GenerateTest { test_id: tid })
                .await;

            let out_file_path = format!("{}/{}-in.txt", &tests_path, tid);
            match &test_spec.gen {
                crate::manifest::TestGenSpec::Generate { testgen, args } => {
                    let testgen_cmd = testgens
                        .get(testgen)
                        .with_context(|| format!("error: unknown testgen {}", testgen))?;

                    let mut entropy_buf = [0; crate::manifest::RANDOM_SEED_LENGTH];
                    get_entropy_hex(&mut entropy_buf);
                    let entropy = String::from_utf8(entropy_buf.to_vec()).unwrap(); // only ASCII can be here

                    let mut cmd = testgen_cmd.clone();
                    for a in args {
                        cmd.arg(a);
                    }
                    cmd.env("JJS_TEST_ID", &tid.to_string());
                    cmd.env("JJS_RANDOM_SEED", &entropy);
                    self.configure_command(&mut cmd);
                    let gen_out = cmd.run_quiet().await?;
                    tokio::fs::write(&out_file_path, gen_out.stdout)
                        .await
                        .context("failed to write test")?;
                }
                crate::manifest::TestGenSpec::File { path } => {
                    let src_path = self.problem_dir.join("tests").join(path);
                    if let Err(e) = tokio::fs::copy(&src_path, &out_file_path).await {
                        anyhow::bail!(
                            "Couldn't copy test data from {} to {}: {}",
                            src_path.display(),
                            out_file_path,
                            e,
                        );
                    }
                }
            }
            let mut test_info = pom::Test {
                path: FileRef {
                    path: format!("tests/{}-in.txt", tid),
                    root: FileRefRoot::Problem,
                },
                correct: None,
                limits: merge_limits(&[self.cfg.limits, test_spec.limits]),
                group: test_spec.group.clone(),
            };
            if let Some(cmd) = gen_answers {
                let test_data = tokio::fs::File::open(&out_file_path).await?;

                let correct_file_path = format!("{}/{}-out.txt", &tests_path, tid);

                let answer_data = tokio::fs::File::create(&correct_file_path).await?;

                let mut cmd = cmd.clone();
                self.configure_command(&mut cmd);
                let mut cmd = cmd.to_tokio_command();
                let mut close_handles = vec![];
                unsafe {
                    let test_data_fd = test_data.into_std().await.into_raw_fd();
                    close_handles.push(test_data_fd);
                    let test_data_fd = libc::dup(test_data_fd);
                    close_handles.push(test_data_fd);

                    let ans_data_fd = answer_data.into_std().await.into_raw_fd();
                    close_handles.push(ans_data_fd);
                    let ans_data_fd = libc::dup(ans_data_fd);
                    close_handles.push(ans_data_fd);
                    cmd.pre_exec(move || {
                        if libc::dup2(test_data_fd, 0) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::dup2(ans_data_fd, 1) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                let output = cmd
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .await
                    .context("launch main solution error: {}")?;
                if !output.status.success() {
                    anyhow::bail!(
                        "Error while generating correct answer for test {}: main solution failed: {}",
                        tid,
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                let short_file_path = format!("tests/{}-out.txt", tid);
                test_info.correct.replace(FileRef {
                    path: short_file_path,
                    root: FileRefRoot::Problem,
                });
                for handle in close_handles {
                    unsafe {
                        libc::close(handle);
                    }
                }
            }
            out.push(test_info);
        }
        Ok(out)
    }

    /// Builds all checkers (currently only one is supported)
    async fn build_checkers(&mut self) -> anyhow::Result<FileRef> {
        // TODO: support multi-file checkers
        let checker_path = format!("{}/checkers/main.cpp", self.problem_dir.display());
        self.build_checker(&checker_path).await
    }

    /// Builds single checker
    async fn build_checker(&mut self, checker_path: &str) -> anyhow::Result<FileRef> {
        let out_path = self.out_dir.join("assets/checker");
        self.pw.send(CompileUpdate::BuildChecker).await;
        match &self.cfg.check {
            crate::manifest::Check::Custom(_) => {
                self.do_build(Path::new(checker_path), Path::new(&out_path))
                    .await?;
                Ok(FileRef {
                    path: "checker/bin".to_string(),
                    root: FileRefRoot::Problem,
                })
            }
            crate::manifest::Check::Builtin(bc) => {
                let src_path = self
                    .build_env
                    .join(format!("bin/builtin-checker-{}", bc.name));
                tokio::fs::create_dir(&out_path)
                    .await
                    .context("failed to create out directory")?;
                tokio::fs::copy(&src_path, &out_path.join("bin"))
                    .await
                    .context("failed to copy checker binary")?;
                Ok(FileRef {
                    path: "checker/bin".to_string(),
                    root: FileRefRoot::Problem,
                })
            }
        }
    }

    /// Builds all modules
    ///
    /// Module is user-defined program. PPC only builds module and places
    /// binaries into compiled problem assets.
    async fn build_modules(&self) -> anyhow::Result<()> {
        for module in self.glob("modules/*").await? {
            let module_name = module.file_name().unwrap().to_str().expect("utf8 error");
            let output_path = self
                .out_dir
                .join("assets")
                .join(format!("module-{}", module_name));
            self.do_build(&module, Path::new(&output_path)).await?;
        }
        Ok(())
    }

    /// Copies files that should just be copied as is.
    /// Currently, only such file is valuer config
    async fn copy_raw(&mut self) -> anyhow::Result<()> {
        let valuer_cfg_dir = self.out_dir.join("assets/valuer-cfg");
        if let Some(valuer_cfg) = &self.cfg.valuer_cfg {
            self.pw.send(CompileUpdate::CopyValuerConfig).await;

            let src = self.problem_dir.join(valuer_cfg.trim_start_matches('/'));
            let dest = valuer_cfg_dir.join("cfg.yaml");
            tokio::fs::create_dir(&valuer_cfg_dir).await?;
            if src.is_file() {
                tokio::fs::copy(&src, &dest).await?;
            } else {
                // TODO
                anyhow::bail!("Multi-file valuer config is TODO");
            }
        }
        Ok(())
    }

    /// Main method, which actually builds the problem into
    /// redistributable package.
    pub async fn build(&mut self) -> anyhow::Result<()> {
        self.build_modules().await?;
        let solutions = self.build_solutions().await?;
        let testgen_launch_info = self.build_testgens().await?;

        let checker_ref = self
            .build_checkers()
            .await
            .context("failed to build checker")?;

        let checker_cmd = self.cfg.check_options.args.clone();

        let tests = {
            let gen_answers = match &self.cfg.check {
                crate::manifest::Check::Custom(cs) => cs.pass_correct,
                crate::manifest::Check::Builtin(_) => true,
            };
            let gen_answers = if gen_answers {
                let primary_solution_name = self.cfg.primary_solution.as_ref().context(
                    "primary-solution must be specified in order to generate tests correct answers",
                )?;
                let sol_data = match solutions.get(primary_solution_name.as_str()) {
                    Some(d) => d,
                    None => {
                        eprint!("Following solutions are defined: ");
                        for sol_name in solutions.keys() {
                            eprint!("{} ", sol_name);
                        }
                        anyhow::bail!("Unknown solution {}", primary_solution_name)
                    }
                };
                Some(sol_data)
            } else {
                None
            };
            self.build_tests(&testgen_launch_info, gen_answers).await?
        };
        self.copy_raw().await?;

        let valuer_exe = {
            let src = self.build_env.join("bin/svaluer");
            let dest = self.out_dir.join("assets/valuer");
            tokio::fs::copy(&src, &dest)
                .await
                .context("failed to copy valuer binary")?;
            FileRef {
                root: FileRefRoot::Problem,
                path: "valuer".to_string(),
            }
        };
        let valuer = pom::ChildValuer {
            exe: valuer_exe,
            extra_args: Vec::new(),
        };

        let valuer_config = FileRef {
            root: FileRefRoot::Problem,
            path: "valuer-cfg".to_string(),
        };

        let problem = pom::Problem {
            title: self.cfg.title.clone(),
            name: self.cfg.name.clone(),
            checker_exe: checker_ref,
            checker_cmd,
            valuer: pom::Valuer::Child(valuer),
            tests,
            valuer_config,
        };
        let manifest_path = format!("{}/manifest.json", self.out_dir.display());
        let manifest_data =
            serde_json::to_string(&problem).context("couldn't serialize manifest")?;
        std::fs::write(manifest_path, manifest_data).context("couldn't emit manifest")
    }
}
