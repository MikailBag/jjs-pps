//! Simple valuer
use anyhow::Context;
use log::debug;
use pom::TestId;
use std::collections::HashSet;

/// CLI-based driver, useful for manual testing valuer config
#[derive(Debug)]
struct TermDriver {
    current_tests: HashSet<TestId>,
    full_judge_log: Option<valuer_api::JudgeLog>,
}

mod term_driver {
    use super::TermDriver;
    use anyhow::{Context, Result};
    use pom::TestId;
    use std::{
        io::{stdin, stdout, Write},
        str::FromStr,
    };
    fn read_value<T: FromStr>(what: impl AsRef<str>) -> Result<T>
    where
        <T as FromStr>::Err: std::error::Error,
    {
        let mut user_input = String::new();
        loop {
            print!("{}> ", what.as_ref());
            stdout().flush()?;
            user_input.clear();
            stdin()
                .read_line(&mut user_input)
                .context("failed to read line")?;
            let user_input = user_input.trim();
            match user_input.parse() {
                // These are different Ok's: one is anyhow::Result::Ok, other is Result<.., <T as FromStr>::Err>>
                Ok(x) => break Ok(x),
                Err(err) => {
                    eprintln!("failed to parse your input: {}. Please, enter again.", err);
                    continue;
                }
            }
        }
    }

    impl svaluer::ValuerDriver for TermDriver {
        fn problem_info(&mut self) -> Result<valuer_api::ProblemInfo> {
            let test_count = read_value("test count")?;
            let mut tests = Vec::new();
            for i in 1..=test_count {
                let group = read_value(format!("group test #{} belongs to", i))?;
                tests.push(group);
            }
            let info = valuer_api::ProblemInfo { tests };
            Ok(info)
        }

        fn send_command(&mut self, resp: &valuer_api::ValuerResponse) -> Result<()> {
            match resp {
                valuer_api::ValuerResponse::Finish => {
                    let judge_log = self.full_judge_log.take().expect("full judge log missing");

                    println!("Judging finished");
                    println!("Score: {}", judge_log.score);
                    if judge_log.is_full {
                        println!("Full solution");
                    } else {
                        println!("Partial solution");
                    }
                }
                valuer_api::ValuerResponse::LiveScore { score } => {
                    println!("Current score: {}", *score);
                }
                valuer_api::ValuerResponse::Test { test_id, live } => {
                    println!("Run should be executed on test {}", test_id.get());
                    if *live {
                        println!("Current test: {}", test_id.get());
                    }
                    let not_dup = self.current_tests.insert(*test_id);
                    assert!(not_dup);
                }
                valuer_api::ValuerResponse::JudgeLog { .. } => {
                    // TODO print judge log
                }
            }
            Ok(())
        }

        fn poll_notification(&mut self) -> Result<Option<valuer_api::TestDoneNotification>> {
            fn create_status(ok: bool) -> valuer_api::Status {
                if ok {
                    svaluer::status_util::make_ok_status()
                } else {
                    svaluer::status_util::make_err_status()
                }
            }

            fn read_status(tid: TestId) -> Result<valuer_api::TestDoneNotification> {
                let outcome = read_value(format!("test {} status", tid.get()))?;
                let test_status = create_status(outcome);
                Ok(valuer_api::TestDoneNotification {
                    test_id: tid,
                    test_status,
                })
            }
            match self.current_tests.len() {
                0 => Ok(None),
                1 => {
                    let tid = self.current_tests.drain().next().unwrap();
                    Ok(Some(read_status(tid)?))
                }
                _ => {
                    let test_id = loop {
                        let tid: std::num::NonZeroU32 = read_value("next finished test")?;
                        if !self.current_tests.remove(&TestId(tid)) {
                            eprintln!(
                                "Test {} was already finished or is not requested to run",
                                tid.get()
                            );
                            eprintln!("Current tests: {:?}", &self.current_tests);
                            continue;
                        }
                        break TestId(tid);
                    };
                    Ok(Some(read_status(test_id)?))
                }
            }
        }
    }
}

use json_driver::JsonDriver;

mod json_driver {
    use anyhow::{bail, Context, Result};
    use serde::Deserialize;
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    use svaluer::ValuerDriver;
    /// Json-RPC driver, used in integration with JJS invoker
    #[derive(Debug)]
    pub struct JsonDriver {
        chan: crossbeam_channel::Receiver<Message>,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Message {
        ProblemInfo(valuer_api::ProblemInfo),
        TestDoneNotify(valuer_api::TestDoneNotification),
    }
    fn json_driver_thread_func(chan: crossbeam_channel::Sender<Message>) {
        let mut buf = String::new();
        loop {
            buf.clear();
            if let Err(err) = std::io::stdin().read_line(&mut buf) {
                eprintln!("svaluer: fatal: io error: {}", err);
                break;
            }
            let notify = match serde_json::from_str(&buf) {
                Ok(val) => val,
                Err(err) => {
                    eprintln!(
                        "svaluer: error: failed to deserialize invoker TestDoneNotification: {}",
                        err
                    );
                    continue;
                }
            };
            if chan.send(notify).is_err() {
                // we get error, if receiver is closed. It means we should stop.
                break;
            }
        }
    }
    const WAIT_TIMEOUT: Duration = Duration::from_millis(100);
    impl JsonDriver {
        pub fn new() -> Self {
            let (send, recv) = crossbeam_channel::unbounded();
            std::thread::spawn(move || {
                json_driver_thread_func(send);
            });
            Self { chan: recv }
        }

        fn poll(&mut self) -> Option<Message> {
            match self.chan.recv_timeout(WAIT_TIMEOUT) {
                Ok(msg) => Some(msg),
                Err(_err) => None,
            }
        }
    }

    impl ValuerDriver for JsonDriver {
        fn problem_info(&mut self) -> Result<valuer_api::ProblemInfo> {
            let begin_time = Instant::now();
            const TIMEOUT: Duration = Duration::from_secs(1);
            let message = loop {
                if let Some(msg) = self.poll() {
                    break msg;
                }
                if Instant::now().duration_since(begin_time) > TIMEOUT {
                    bail!("timeout");
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            let problem_info = match message {
                Message::ProblemInfo(pi) => pi,
                Message::TestDoneNotify(tdn) => bail!("got TestDoneNotification {:?} instead", tdn),
            };
            Ok(problem_info)
        }

        fn send_command(&mut self, cmd: &valuer_api::ValuerResponse) -> Result<()> {
            let cmd = serde_json::to_string(cmd).context("failed to serialize")?;
            println!("{}", cmd);
            std::io::stdout().flush().context("failed to flush")?;
            Ok(())
        }

        fn poll_notification(&mut self) -> Result<Option<valuer_api::TestDoneNotification>> {
            match self.poll() {
                None => Ok(None),
                Some(msg) => match msg {
                    Message::TestDoneNotify(tdn) => Ok(Some(tdn)),
                    Message::ProblemInfo(pi) => bail!("got ProblemInfo {:?} instead", pi),
                },
            }
        }
    }
}

fn parse_config() -> anyhow::Result<svaluer::cfg::Config> {
    let path = std::path::Path::new("cfg.yaml");
    let data = std::fs::read_to_string(path).context("failed to read cfg.yaml")?;
    Ok(serde_yaml::from_str(&data).context("failed to parse config")?)
}

fn main_cli_mode() -> anyhow::Result<()> {
    let mut driver = TermDriver {
        current_tests: HashSet::new(),
        full_judge_log: None,
    };
    let cfg = parse_config()?;
    let valuer = svaluer::SimpleValuer::new(&mut driver, &cfg)?;
    valuer.exec()
}

fn main_json_mode() -> anyhow::Result<()> {
    let mut driver = JsonDriver::new();
    let cfg = parse_config()?;
    let valuer = svaluer::SimpleValuer::new(&mut driver, &cfg)?;
    valuer.exec()
}

fn main() -> anyhow::Result<()> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info,svaluer=debug");
    }

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let json_mode = std::env::var("JJS_VALUER").is_ok();
    if json_mode {
        debug!("Mode: JSON");
        main_json_mode()?
    } else {
        debug!("Mode: CLI");
        main_cli_mode()?
    }

    Ok(())
}
