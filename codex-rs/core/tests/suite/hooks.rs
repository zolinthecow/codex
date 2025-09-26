#![cfg(not(target_os = "windows"))]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::Context;
use codex_core::config::HookRule;
use codex_core::config::HookToolMatcher;
use codex_core::config::HooksConfig;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::InputItem;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::config_types::ReasoningSummary;
use core_test_support::non_sandbox_test;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use responses::ev_assistant_message;
use responses::ev_completed;
use responses::ev_function_call;
use responses::sse;
use responses::start_mock_server;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::sleep;
use wiremock::matchers::any;

const MODEL_NAME: &str = "gpt-5";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_prompt_submit_hook_records_payload() -> anyhow::Result<()> {
    non_sandbox_test!(result);

    let server = start_mock_server().await;

    let sse_body = sse(vec![ev_assistant_message("m1", "done"), ev_completed("r1")]);
    responses::mount_sse_once(&server, any(), sse_body).await;

    let hook_tmp = TempDir::new()?;
    let log_path = hook_tmp.path().join("user_prompt.log");
    let script_path = write_hook_script(
        hook_tmp.path(),
        "user_prompt.sh",
        &format!(
            r#"#!/bin/bash
set -euo pipefail
printf '%s\n' "${{@: -1}}" >> "{}"
"#,
            log_path.display()
        ),
    )?;

    let hook_cfg = HooksConfig {
        user_prompt_submit: Some(vec![script_path.to_string_lossy().into_owned()]),
        timeout_ms: 2_000,
        ..HooksConfig::default()
    };

    let TestCodexContext { codex, cwd, .. } = build_codex_with_hooks(&server, hook_cfg).await?;

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello world".into(),
            }],
        })
        .await?;

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let entries = read_hook_entries(&log_path).await?;
    assert_eq!(entries.len(), 1, "expected a single hook invocation");
    let payload = &entries[0];
    assert_eq!(payload["type"], Value::String("user-prompt-submit".into()));
    assert_eq!(
        payload["texts"],
        Value::Array(vec![Value::String("hello world".into())])
    );
    assert_eq!(payload["images"], Value::Array(vec![]));
    assert_eq!(
        payload["cwd"],
        Value::String(cwd.path().to_string_lossy().into())
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_tool_hook_failure_blocks_execution() -> anyhow::Result<()> {
    non_sandbox_test!(result);

    let server = start_mock_server().await;

    let _tmp_dir = TempDir::new()?;
    let command_output = _tmp_dir.path().join("should_not_exist.txt");
    let command = format!("echo ran > {}", command_output.display());
    let args = shell_args(&command);
    let sse_body = sse(vec![
        ev_function_call("call-1", "container.exec", &args),
        ev_completed("r1"),
    ]);
    responses::mount_sse_once(&server, any(), sse_body).await;

    let hook_tmp = TempDir::new()?;
    let log_path = hook_tmp.path().join("pre_hook.log");
    let script_path = write_hook_script(
        hook_tmp.path(),
        "pre_fail.sh",
        &format!(
            r#"#!/bin/bash
set -euo pipefail
printf '%s\n' "${{@: -1}}" >> "{}"
exit 42
"#,
            log_path.display()
        ),
    )?;

    let hook_rule = HookRule {
        argv: vec![script_path.to_string_lossy().into_owned()],
        matcher: HookToolMatcher::default(),
    };

    let hook_cfg = HooksConfig {
        pre_tool_use_rules: vec![hook_rule],
        timeout_ms: 2_000,
        ..HooksConfig::default()
    };

    let TestCodexContext { codex, cwd, .. } = build_codex_with_hooks(&server, hook_cfg).await?;

    codex
        .submit(Op::UserTurn {
            items: vec![InputItem::Text {
                text: "please run".into(),
            }],
            cwd: cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: MODEL_NAME.into(),
            effort: None,
            summary: ReasoningSummary::Auto,
            final_output_json_schema: None,
        })
        .await?;

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // Hook script should have executed and the command should not have run.
    let entries = read_hook_entries(&log_path).await?;
    assert_eq!(entries.len(), 1, "expected a single pre-hook invocation");
    assert!(
        !command_output.exists(),
        "pre-hook failure should block command execution"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_tool_hook_captures_output_metadata() -> anyhow::Result<()> {
    non_sandbox_test!(result);

    let server = start_mock_server().await;

    let args = shell_args("echo hook-output");
    let sse_body = sse(vec![
        ev_function_call("call-1", "container.exec", &args),
        ev_completed("r1"),
    ]);
    responses::mount_sse_once(&server, any(), sse_body).await;

    let hook_tmp = TempDir::new()?;
    let post_log = hook_tmp.path().join("post_hook.log");
    let post_script = write_hook_script(
        hook_tmp.path(),
        "post.sh",
        &format!(
            r#"#!/bin/bash
set -euo pipefail
printf '%s\n' "${{@: -1}}" >> "{}"
"#,
            post_log.display()
        ),
    )?;

    let pre_log = hook_tmp.path().join("pre_hook.log");
    let pre_script = write_hook_script(
        hook_tmp.path(),
        "pre_ok.sh",
        &format!(
            r#"#!/bin/bash
set -euo pipefail
printf '%s\n' "${{@: -1}}" >> "{}"
"#,
            pre_log.display()
        ),
    )?;

    let hook_cfg = HooksConfig {
        pre_tool_use_rules: vec![HookRule {
            argv: vec![pre_script.to_string_lossy().into_owned()],
            matcher: HookToolMatcher::default(),
        }],
        post_tool_use_rules: vec![HookRule {
            argv: vec![post_script.to_string_lossy().into_owned()],
            matcher: HookToolMatcher::default(),
        }],
        timeout_ms: 2_000,
        ..HooksConfig::default()
    };

    let TestCodexContext { codex, cwd, .. } = build_codex_with_hooks(&server, hook_cfg).await?;

    codex
        .submit(Op::UserTurn {
            items: vec![InputItem::Text {
                text: "run shell".into(),
            }],
            cwd: cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: MODEL_NAME.into(),
            effort: None,
            summary: ReasoningSummary::Auto,
            final_output_json_schema: None,
        })
        .await?;

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let entries = read_hook_entries(&post_log).await?;
    assert_eq!(entries.len(), 1, "expected a single post-hook invocation");
    let payload = &entries[0];
    assert_eq!(payload["type"], Value::String("post-tool-use".into()));
    assert_eq!(payload["tool"], Value::String("shell".into()));
    assert_eq!(payload["success"], Value::Bool(true));
    let output = payload
        .get("output")
        .and_then(|v| v.as_str())
        .context("post hook payload missing output field")?;
    assert!(output.contains("hook-output"));

    Ok(())
}

struct TestCodexContext {
    codex: std::sync::Arc<codex_core::CodexConversation>,
    cwd: TempDir,
    _home: TempDir,
}

async fn build_codex_with_hooks(
    server: &wiremock::MockServer,
    hooks: HooksConfig,
) -> anyhow::Result<TestCodexContext> {
    use core_test_support::test_codex::TestCodex;

    let hooks_cfg = hooks.clone();
    let mut builder = test_codex().with_config(move |cfg| {
        cfg.hooks = hooks_cfg.clone();
    });

    let TestCodex {
        codex, cwd, home, ..
    } = builder.build(server).await?;
    // Drain the SessionConfigured event so tests can focus on their assertions.
    let _ = wait_for_event(&codex, |ev| matches!(ev, EventMsg::SessionConfigured(_))).await;

    Ok(TestCodexContext {
        codex,
        cwd,
        _home: home,
    })
}

fn shell_args(command: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "command": ["/bin/bash", "-c", command],
        "workdir": null,
        "timeout_ms": null,
        "with_escalated_permissions": null,
        "justification": null,
    }))
    .expect("serialize shell arguments")
}

fn write_hook_script(dir: &Path, name: &str, body: &str) -> anyhow::Result<std::path::PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, body).with_context(|| format!("write script {path:?}"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod script {path:?}"))?;
    Ok(path)
}

async fn read_hook_entries(path: &Path) -> anyhow::Result<Vec<Value>> {
    const ATTEMPTS: usize = 50;
    for _ in 0..ATTEMPTS {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let entries = contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).map_err(anyhow::Error::from))
                .collect::<Result<Vec<_>, _>>()?;
            if !entries.is_empty() {
                return Ok(entries);
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("timed out waiting for hook log at {}", path.display())
}
