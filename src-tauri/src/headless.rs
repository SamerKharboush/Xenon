//! Headless smoke-test entry: `xenon --agent-task "<task>" [--profile <name>]`
//! runs one agent turn without the pet UI and prints every step to stdout.
//! Exit code 0 on `done`, 1 on `fail` / error / no terminal action.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::agent::Agent;
use crate::config::XenonConfig;
use crate::events::{AgentAction, PetState};
use crate::runtime::build_agent_async;

/// Parse `--agent-task` / `--profile`. `Some(code)` means headless mode ran and
/// this is the process exit code.
pub fn run_from_args(cfg: &XenonConfig) -> Option<i32> {
    let mut args = std::env::args().skip(1);
    let mut task: Option<String> = None;
    let mut profile: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--agent-task" => task = args.next(),
            "--profile" => profile = args.next(),
            _ => {}
        }
    }
    task.map(|task| run(&task, profile.as_deref(), cfg))
}

fn run(task: &str, profile: Option<&str>, cfg: &XenonConfig) -> i32 {
    let perms = crate::permissions::get_permissions();
    if !perms.accessibility.granted || !perms.screen_recording.granted {
        eprintln!(
            "warning: macOS permissions missing (accessibility: {}, screen recording: {}). \
             Desktop steps will fail until both are granted in System Settings → \
             Privacy & Security.",
            perms.accessibility.granted, perms.screen_recording.granted
        );
    }

    let mut cfg = cfg.clone();
    if let Some(name) = profile {
        if !cfg.providers.iter().any(|p| p.name == name) {
            eprintln!("error: no provider profile named {name:?}");
            return 1;
        }
        cfg.roles.agent = name.to_string();
        cfg.roles.chat = name.to_string();
        cfg.roles.vision = name.to_string();
    }

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: could not start the async runtime: {e}");
            return 1;
        }
    };

    let outcome = Arc::new(Mutex::new(Outcome::Incomplete));
    let outcome_reader = outcome.clone();

    rt.block_on(async move {
        let agent: Agent = build_agent_async(&cfg, None, Arc::new(AtomicBool::new(false))).await;
        let (tx, rx) = async_channel::bounded(64);

        let printer_outcome = outcome.clone();
        let printer = tokio::spawn(async move {
            let mut step_start: Option<std::time::Instant> = None;
            while let Ok(action) = rx.recv().await {
                match action {
                    AgentAction::SetPetState(state) => {
                        if state != PetState::Agentic {
                            println!("state: {state:?}");
                        }
                    }
                    AgentAction::Speak(text) => println!("speak: {text}"),
                    AgentAction::ExecuteTool(call) => {
                        step_start = Some(std::time::Instant::now());
                        println!("action: {} {}", call.name, call.args);
                        if call.name == "done" {
                            *printer_outcome.lock().unwrap() = Outcome::Done;
                        } else if call.name == "fail" {
                            *printer_outcome.lock().unwrap() = Outcome::Failed;
                        }
                    }
                    AgentAction::ToolResult(result) => {
                        if let Some(t0) = step_start.take() {
                            println!("  took: {:.2}s", t0.elapsed().as_secs_f64());
                        }
                        if result.is_error {
                            println!(
                                "  ! error: {}",
                                result
                                    .error_message
                                    .clone()
                                    .unwrap_or_else(|| result.result.to_string())
                            );
                        } else {
                            println!("  result: {}", compact(&result.result.to_string()));
                        }
                    }
                    AgentAction::TurnFinished => println!("turn finished"),
                }
            }
        });

        println!("task: {task}");
        let agent_outcome = outcome.clone();
        if let Err(e) = agent.run_utterance(task, tx).await {
            eprintln!("error: agent turn failed: {e}");
            let mut out = agent_outcome.lock().unwrap();
            if *out == Outcome::Incomplete {
                *out = Outcome::Failed;
            }
        }
        let _ = printer.await;
    });

    let outcome = *outcome_reader.lock().unwrap();
    match outcome {
        Outcome::Done => {
            println!("exit: 0 (done)");
            0
        }
        Outcome::Failed => {
            println!("exit: 1 (fail)");
            1
        }
        Outcome::Incomplete => {
            println!("exit: 1 (no done/fail action reached)");
            1
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    Incomplete,
}

fn compact(s: &str) -> String {
    if s.chars().count() <= 400 {
        return s.to_string();
    }
    s.chars().take(400).collect::<String>() + "…"
}