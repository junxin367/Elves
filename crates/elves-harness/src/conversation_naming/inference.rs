use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use super::super::HarnessAuthorization;
use super::super::antigravity::acp::EnvConfig as AntigravityConfig;
use super::super::claude::{Claude, WslConfig};
use super::Settings;

const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OUTPUT: u64 = 256 * 1024;

fn schema() -> Value {
    json!({"type":"object","properties":{"title":{"type":"string"},"description":{"type":"string"}},
        "required":["title","description"],"additionalProperties":false})
}

async fn write(
    service: &crate::codex_service::RunningService,
    frame: Value,
    authorization: &HarnessAuthorization,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(&frame)?;
    bytes.push(b'\n');
    crate::service_transport::guarded_write(&service.stdin, bytes, Some(authorization), TIMEOUT)
        .await
}

async fn rpc(
    service: &mut crate::codex_service::RunningService,
    id: u64,
    method: &str,
    params: Value,
    authorization: &HarnessAuthorization,
    mut events: Option<&mut std::collections::VecDeque<Value>>,
) -> anyhow::Result<Value> {
    write(
        service,
        json!({"id":id,"method":method,"params":params}),
        authorization,
    )
    .await?;
    loop {
        let frame = frame(service).await?;
        if frame["id"] != id {
            if let Some(events) = events.as_mut() {
                anyhow::ensure!(events.len() < 256, "命名引擎提前返回过多事件");
                events.push_back(frame);
            }
            continue;
        }
        if frame.get("error").is_some() {
            bail!(
                "命名引擎拒绝请求：{}",
                frame["error"]["message"].as_str().unwrap_or("原生请求失败")
            );
        }
        return Ok(frame["result"].clone());
    }
}

async fn frame(service: &mut crate::codex_service::RunningService) -> anyhow::Result<Value> {
    let mut line = String::new();
    let bytes = (&mut service.stdout)
        .take(MAX_OUTPUT + 1)
        .read_line(&mut line)
        .await?;
    anyhow::ensure!(
        bytes > 0 && bytes as u64 <= MAX_OUTPUT,
        "命名引擎已退出或输出超过限制"
    );
    Ok(serde_json::from_str(&line)?)
}

pub(super) async fn codex(
    settings: &Settings,
    input: &Value,
    effort: Option<&str>,
    authorization: &HarnessAuthorization,
) -> anyhow::Result<String> {
    // 命名只用独立 stdio 进程，不进入 Elves 私有 Codex 服务（见 spawn_inference_service）。
    let service = crate::codex_service::spawn_inference_service().await?;
    codex_with_service(service, settings, input, effort, authorization).await
}

async fn codex_with_service(
    mut service: crate::codex_service::RunningService,
    settings: &Settings,
    input: &Value,
    effort: Option<&str>,
    authorization: &HarnessAuthorization,
) -> anyhow::Result<String> {
    let outcome = tokio::time::timeout(TIMEOUT, async {
        rpc(&mut service, 1, "initialize", json!({"clientInfo":{"name":crate::protocol_proxy::CONVERSATION_NAMING_CLIENT,
            "version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}), authorization, None).await?;
        write(&service, json!({"method":"initialized","params":{}}), authorization).await?;
        let saved = rpc(&mut service, 2, "config/read", json!({"includeLayers":false}), authorization, None).await?;
        let disabled_mcp = saved["config"]["mcp_servers"].as_object().into_iter().flatten()
            .map(|(name,_)| (name.clone(),json!({"enabled":false}))).collect::<serde_json::Map<_,_>>();
        // 用户要求只生成元数据；禁用原生可配置工具，代理开启时另行移除原生仍声明的内置工具。
        let mut config = json!({"project_doc_max_bytes":0,"include_environment_context":false,
            "include_permissions_instructions":false,"include_apps_instructions":false,
            "mcp_servers":disabled_mcp,"apps":{"_default":{"enabled":false}},
            "skills":{"bundled":{"enabled":false},"config":[]},
            "memories":{"use_memories":false,"generate_memories":false},"web_search":"disabled",
            "features.hooks":false,"features.apps":false,"features.plugins":false,
            "features.shell_tool":false,"features.unified_exec":false,"features.apply_patch_freeform":false,
            "features.js_repl":false,"features.code_mode":false,"features.multi_agent":false,
            "features.multi_agent_v2":false,"features.memories":false,"features.image_generation":false,
            "features.goals":false,"features.view_image":false,"features.sleep_tool":false,
            "features.default_mode_request_user_input":false,"features.tool_suggest":false});
        if let Some(effort) = effort { config["model_reasoning_effort"] = json!(effort); }
        let thread = rpc(&mut service, 3, "thread/start", json!({
            "model":settings.model,"ephemeral":true,"threadSource":"thread_title",
            "baseInstructions":format!("{}{}",crate::protocol_proxy::CONVERSATION_NAMING_INSTRUCTIONS_PREFIX,
                settings.system_prompt),"developerInstructions":"",
            "dynamicTools":[],"config":config,
            "approvalPolicy":"never","sandbox":"read-only","cwd":std::env::temp_dir()
        }), authorization, None).await?;
        let id = thread["thread"]["id"].as_str().context("命名引擎未返回临时会话 ID")?.to_owned();
        let mut early = std::collections::VecDeque::new();
        let started = rpc(&mut service, 4, "turn/start", json!({
            "threadId":id,"model":settings.model,"effort":effort,"outputSchema":schema(),
            "input":[{"type":"text","text":format!("{}{}",crate::protocol_proxy::CONVERSATION_NAMING_INPUT_PREFIX,input),
                "text_elements":[]}]
        }), authorization, Some(&mut early)).await?;
        let mut text = String::new();
        if let Some(items) = started["turn"]["items"].as_array() {
            for item in items {
                ensure_naming_item(item)?;
                if item["type"] == "agentMessage" { text = item["text"].as_str().unwrap_or("").into(); }
            }
        }
        loop {
            // 原生完成事件可先于 turn/start 回执；必须按接收顺序处理，不能漏掉已完成的标题结果。
            let frame = if let Some(frame) = early.pop_front() { frame } else { frame(&mut service).await? };
            if frame.get("id").is_some() && frame.get("method").is_some() {
                bail!("命名模型请求了交互或工具，已停止本次命名");
            }
            if frame["params"]["threadId"] != id { continue; }
            if matches!(frame["method"].as_str(), Some("item/started" | "item/completed")) {
                let item = &frame["params"]["item"];
                ensure_naming_item(item)?;
                if frame["method"] == "item/completed" && item["type"] == "agentMessage" {
                    text = item["text"].as_str().unwrap_or("").into();
                }
            }
            if frame["method"] == "turn/plan/updated" {
                bail!("命名模型试图执行原任务，已停止本次命名");
            }
            if frame["method"] == "turn/completed" {
                anyhow::ensure!(frame["params"]["turn"]["status"] == "completed", "命名模型未成功完成");
                anyhow::ensure!(!text.is_empty(), "命名模型未返回标题");
                return Ok(text);
            }
        }
    }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("Codex 会话命名超时")));
    let _ = service.child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), service.child.wait()).await;
    outcome
}

fn ensure_naming_item(item: &Value) -> anyhow::Result<()> {
    // 直连或上游未遵守工具限制时，不接收包含工具或其他操作的命名结果。
    anyhow::ensure!(
        matches!(
            item["type"].as_str(),
            Some("userMessage" | "agentMessage" | "reasoning")
        ),
        "命名模型返回了工具或非文本操作，已停止本次命名"
    );
    Ok(())
}

pub(super) async fn claude(
    config: &WslConfig,
    settings: &Settings,
    input: &Value,
    authorization: &HarnessAuthorization,
) -> anyhow::Result<String> {
    let mut command = Claude::wsl(config)?;
    command.args(["--cd", "~"]);
    Claude::wsl_login_exec(&mut command, &config.executable);
    command.args([
        "--print",
        "--output-format",
        "json",
        "--safe-mode",
        "--no-session-persistence",
        "--model",
        &settings.model,
        "--system-prompt",
        &settings.system_prompt,
        "--tools",
        "",
        "--strict-mcp-config",
        "--mcp-config",
        r#"{"mcpServers":{}}"#,
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ]);
    command.args(["--json-schema", &schema().to_string()]);
    let permit = authorization.prepare_dispatch().await?;
    let mut child = permit.commit(|| {
        Ok(command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?)
    })?;
    let outcome = tokio::time::timeout(TIMEOUT, async {
        let mut writer = child.stdin.take().context("Claude 命名输入不可用")?;
        writer.write_all(input.to_string().as_bytes()).await?;
        writer.shutdown().await?;
        drop(writer);
        let output = child.stdout.take().context("Claude 命名输出不可用")?;
        let mut bytes = Vec::new();
        output.take(MAX_OUTPUT + 1).read_to_end(&mut bytes).await?;
        anyhow::ensure!(bytes.len() as u64 <= MAX_OUTPUT, "Claude 命名输出超过限制");
        anyhow::ensure!(child.wait().await?.success(), "Claude 命名进程未成功结束");
        let value: Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(value["is_error"] != true, "Claude 命名返回失败");
        if value["structured_output"].is_object() {
            Ok(value["structured_output"].to_string())
        } else {
            value["result"]
                .as_str()
                .map(str::to_owned)
                .context("Claude 命名没有返回文本结果")
        }
    })
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("Claude 会话命名超时")));
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    outcome
}

pub(super) async fn antigravity(
    config: &AntigravityConfig,
    settings: &Settings,
    input: &Value,
    authorization: &HarnessAuthorization,
) -> anyhow::Result<String> {
    config.validate()?;
    let prompt = format!(
        "{}\n\n严格只输出符合以下 JSON Schema 的单个 JSON 对象，严禁调用任何工具或命令，不要输出 Markdown 代码块：\n{}\n\n输入记录：\n{}",
        settings.system_prompt,
        schema(),
        input
    );
    let mut command = if config.is_local() {
        let mut cmd = tokio::process::Command::new(&config.executable);
        cmd.current_dir(std::env::temp_dir());
        cmd
    } else {
        anyhow::ensure!(cfg!(windows), "WSL Antigravity 实例需要 Windows WSL2");
        let mut cmd = tokio::process::Command::new("wsl.exe");
        cmd.args([
            "--distribution",
            &config.distribution,
            "--user",
            &config.user,
            "--cd",
            "~",
            "--exec",
            "/bin/bash",
            "-lc",
            "exec \"$@\"",
            "--",
            &config.executable,
        ]);
        cmd
    };
    command.args([
        "-p",
        &prompt,
        "--output-format",
        "json",
        "--model",
        &settings.model,
    ]);
    #[cfg(windows)]
    command.creation_flags(crate::windows_create_hidden_process());
    let permit = authorization.prepare_dispatch().await?;
    let mut child = permit.commit(|| {
        Ok(command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?)
    })?;
    let mut temp_conv_id: Option<String> = None;
    let outcome = tokio::time::timeout(TIMEOUT, async {
        let output = child.stdout.take().context("Antigravity 命名输出不可用")?;
        let mut bytes = Vec::new();
        output.take(MAX_OUTPUT + 1).read_to_end(&mut bytes).await?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_OUTPUT,
            "Antigravity 命名输出超过限制"
        );
        anyhow::ensure!(
            child.wait().await?.success(),
            "Antigravity 命名进程未成功结束"
        );
        let raw_text = String::from_utf8_lossy(&bytes);
        let json_slice = raw_text
            .find('{')
            .map(|idx| &raw_text[idx..])
            .unwrap_or(raw_text.as_ref());
        let value: Value = serde_json::from_str(json_slice.trim())?;
        if let Some(cid) = value["conversation_id"]
            .as_str()
            .filter(|s| crate::antigravity::local_store::valid_native_id(s))
        {
            temp_conv_id = Some(cid.to_owned());
        }
        let response_text = value["response"]
            .as_str()
            .or_else(|| value["result"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("Antigravity 命名未返回文本结果")?;
        let cleaned = response_text
            .strip_prefix("```json")
            .or_else(|| response_text.strip_prefix("```"))
            .and_then(|s| s.strip_suffix("```"))
            .map(str::trim)
            .unwrap_or(response_text);
        Ok(cleaned.to_owned())
    })
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("Antigravity 会话命名超时")));
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    // 清理单次无状态命名调用在 antigravity-cli 下生成的临时会话文件，不污染用户历史列表。
    if let Some(cid) = temp_conv_id {
        let gemini_root = if config.is_local() {
            directories::BaseDirs::new().map(|d| d.home_dir().join(".gemini"))
        } else if !config.user.is_empty() {
            let linux_home = if config.user == "root" {
                "/root/.gemini".to_owned()
            } else {
                format!("/home/{}/.gemini", config.user)
            };
            crate::preferences::wsl_path(&config.distribution, &linux_home).ok()
        } else {
            None
        };
        if let Some(root) = gemini_root {
            let _ = tokio::task::spawn_blocking(move || {
                let cli_root = root.join("antigravity-cli");
                for ext in ["db", "pb"] {
                    let p = cli_root.join("conversations").join(format!("{cid}.{ext}"));
                    if std::fs::symlink_metadata(&p).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file()) {
                        let _ = std::fs::remove_file(&p);
                    }
                }
                let brain = cli_root.join("brain").join(&cid);
                if std::fs::symlink_metadata(&brain).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir()) {
                    let _ = std::fs::remove_dir_all(&brain);
                }
            })
            .await;
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conversation_naming_inference_keeps_events_before_start_reply_and_isolates_tools() {
        let (_directory, authorization) =
            super::super::super::tests::service_dispatch_fixture().await;
        let script = r#"
const readline = require('node:readline');
const out = value => process.stdout.write(JSON.stringify(value)+'\n');
readline.createInterface({input:process.stdin}).on('line', line => {
 const q=JSON.parse(line); if(q.id==null)return;
 if(q.method==='initialize')return out({id:q.id,result:{}});
 if(q.method==='config/read')return out({id:q.id,result:{config:{mcp_servers:{fixture:{enabled:true}}}}});
 if(q.method==='thread/start'){
  const p=q.params,c=p.config;
  if(p.ephemeral!==true||p.sandbox!=='read-only'||c.mcp_servers.fixture.enabled!==false
    ||c['features.hooks']!==false||c['features.shell_tool']!==false||p.baseInstructions.length===0)
   return out({id:q.id,error:{message:'unsafe naming thread'}});
  return out({id:q.id,result:{thread:{id:'isolated-naming'}}});
 }
 if(q.method==='turn/start'){
  out({method:'item/completed',params:{threadId:'isolated-naming',item:{type:'agentMessage',text:'{"title":"验证标题","description":"独立命名"}'}}});
  out({method:'turn/completed',params:{threadId:'isolated-naming',turn:{id:'t',status:'completed'}}});
  return out({id:q.id,result:{turn:{id:'t',items:[]}}});
 }
 out({id:q.id,error:{message:'unexpected request'}});
});
"#;
        let mut command = tokio::process::Command::new("node");
        command
            .args(["-e", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(crate::windows_create_hidden_process());
        let service =
            crate::codex_service::RunningService::for_test(command.spawn().unwrap()).unwrap();
        let settings = Settings {
            model: "fixture-model".into(),
            ..Default::default()
        };
        let output = codex_with_service(
            service,
            &settings,
            &json!({"firstUserMessage":"测试命名"}),
            Some("low"),
            &authorization,
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap()["title"],
            "验证标题"
        );
    }
}
