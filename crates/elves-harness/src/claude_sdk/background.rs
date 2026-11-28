//! 只依据原生任务事件维护后台任务展示，不从普通工具的运行状态推断后台任务。

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Value, json};

#[derive(Clone, Default, PartialEq)]
struct Task {
    run: String,
    tool: String,
    stop_requested: bool,
    kind: String,
    name: String,
    started_at_ms: Option<u64>,
    elapsed_ms: Option<u64>,
    sampled_at_ms: Option<u64>,
    order: u64,
}

impl Task {
    fn describe(&mut self, value: &Value) {
        if !self.run.is_empty() && value["run_id"].as_str().is_some_and(|run|run!=self.run) {
            *self=Task{order:self.order,..Task::default()};
        }
        if let Some(run) = value["run_id"].as_str() { self.run = run.into(); }
        if let Some(tool) = value["tool_use_id"].as_str() { self.tool = tool.into(); }
        if let Some(kind) = value["task_type"].as_str().filter(|s| !s.is_empty()) {
            self.kind = kind.to_owned();
        }
        if let Some(name) = value["description"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
        {
            self.name = name.to_owned();
        }
    }
}

#[derive(Default)]
pub(super) struct BackgroundTasks {
    active: BTreeMap<String, Task>,
    // 前台命令稍后转入后台时仍沿用原生开始事件；前台任务本身不进入菜单和后台计数。
    starts: BTreeMap<String, Task>,
    // 只暂存最近终态，以便任务通知先于 JSONL 的原工具记录到达；不参与菜单计数或进程保活。
    finished: VecDeque<Value>,
    // 原生列表移除可能早于结束通知。仅保留身份用于核对后来的回执，不算活动任务。
    retired: VecDeque<(String,Task)>,
    sequence: u64,
}

impl BackgroundTasks {
    pub(super) fn can_control(&self, id: &str, run: &str) -> bool {
        self.active.get(id).is_some_and(|task| !run.is_empty() && task.run == run && task.kind == "local_bash")
    }
    pub(super) fn finished_shell(&self,id:&str,run:&str)->bool {
        !run.is_empty() && self.finished.iter().any(|r|r["taskId"]==id && r["runId"]==run && r["kind"]=="local_bash")
    }
    pub(super) fn save_output(&mut self,id:&str,run:&str,response:&Value)->bool {
        let Some(r)=self.finished.iter_mut().find(|r|r["taskId"]==id && r["runId"]==run && r["kind"]=="local_bash") else{return false;};
        r["outputPending"] = json!(false);
        let Some(s)=response["output"].as_str() else{
            r["outputError"]=response["error"].clone();
            return true;
        };
        // 64 个回执只存每项最后 16 KiB 原文，截取从字符边界开始，界面明确标注。
        let (s,cut)=crate::background_tasks::bounded_tail(s,16*1024);
        r["output"]=json!(s);r["outputTruncated"]=json!(cut||response["truncated"]==true);
        true
    }
    pub(super) fn stop_marker(&mut self,id:&str,run:&str,value:bool)->bool {
        if !self.can_control(id,run){return false;}
        let task=self.active.get_mut(id).unwrap();
        if task.stop_requested==value{return false;}
        task.stop_requested=value;
        if let Some(start)=self.starts.get_mut(id).filter(|s|s.run==run){start.stop_requested=value;}
        true
    }
    pub(super) fn clear(&mut self) {
        self.active.clear();
        self.starts.clear();
        self.retired.clear();
        // CLI 退出会使运行列表失效，但不会撤销已收到的终态；最后两帧可能合并进同一快照。
        self.sequence = 0;
    }

    pub(super) fn has_tasks(&self) -> bool {
        !self.active.is_empty()
    }

    fn task(&mut self, id: &str) -> Task {
        if let Some(task) = self.starts.get(id).or_else(|| self.active.get(id)) {
            return task.clone();
        }
        self.sequence = self.sequence.saturating_add(1);
        Task {
            order: self.sequence,
            ..Task::default()
        }
    }

    pub(super) fn observe(&mut self, frame: &Value, now: u64) -> bool {
        match frame["subtype"].as_str() {
            Some("background_tasks_changed") => {
                let Some(tasks) = frame["tasks"].as_array() else {
                    return false;
                };
                if tasks.iter().any(|task| task_id(task).is_none()) {
                    return false;
                }
                let mut next = BTreeMap::new();
                for value in tasks {
                    let Some(id) = task_id(value) else { continue };
                    if value["ambient"] == true {
                        continue;
                    }
                    if let Some(run)=value["run_id"].as_str()
                        && self.finished.iter().any(|r|r["taskId"]==id && r["runId"]==run) {continue;}
                    let mut task = self.task(id);
                    task.describe(value);
                    task.started_at_ms = task.started_at_ms.or_else(|| {
                        value["_elvesTaskStartedAtMs"]
                            .as_u64()
                            .filter(|value| *value > 0)
                    });
                    next.insert(id.to_owned(), task);
                }
                for (id,old) in self.active.iter().filter(|(id,_)| !next.contains_key(*id)) {
                    if self.retired.len()==64 {self.retired.pop_front();}
                    self.retired.push_back((id.clone(),old.clone()));
                    self.starts.remove(id);
                }
                let changed = self.active != next;
                self.active = next;
                changed
            }
            Some("task_started") => {
                let Some(id) = task_id(frame) else {
                    return false;
                };
                if let Some(run)=frame["run_id"].as_str()
                    && self.finished.iter().any(|r|r["taskId"]==id && r["runId"]==run) {return false;}
                self.finished.retain(|receipt| receipt["taskId"] != id);
                if frame["ambient"] == true {
                    self.starts.remove(id);
                    return self.active.remove(id).is_some();
                }
                let mut task = self.task(id);
                task.describe(frame);
                // 用户要求展示执行时间：仅使用原生开始时间或宿主首次收到开始事件的时间。
                // 重连重放不能使用本次收到帧的 now，否则会把运行已久的任务重置为零。
                task.started_at_ms = task.started_at_ms.or_else(|| {
                    frame["timestamp"]
                        .as_str()
                        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                        .and_then(|value| u64::try_from(value.timestamp_millis()).ok())
                        .filter(|value| *value > 0)
                        .or_else(|| {
                            frame["_elvesTaskStartedAtMs"]
                                .as_u64()
                                .filter(|value| *value > 0)
                        })
                });
                self.starts.insert(id.to_owned(), task.clone());
                if frame["is_backgrounded"] == false {
                    let changed = self.active.remove(id).is_some();
                    self.trim_starts();
                    return changed;
                }
                let changed = self.active.get(id) != Some(&task);
                self.active.insert(id.to_owned(), task);
                self.trim_starts();
                changed
            }
            Some("task_progress") => {
                let Some(id) = task_id(frame) else {
                    return false;
                };
                let Some(task) = self.active.get_mut(id) else {
                    return false;
                };
                if task.name.is_empty() {
                    task.describe(frame);
                }
                if let Some(elapsed) = frame["usage"]["duration_ms"].as_u64() {
                    task.elapsed_ms = Some(elapsed);
                    task.sampled_at_ms = Some(now);
                    if let Some(start) = self.starts.get_mut(id) {
                        start.elapsed_ms = task.elapsed_ms;
                        start.sampled_at_ms = task.sampled_at_ms;
                    }
                }
                true
            }
            Some("task_notification")
                if matches!(
                    frame["status"].as_str(),
                    Some("completed" | "failed" | "stopped" | "killed" | "cancelled" | "canceled")
                ) =>
            {
                let Some(id) = task_id(frame) else {
                    return false;
                };
                if self.active.get(id).is_some_and(|t| !t.run.is_empty() && frame["run_id"].as_str().is_some_and(|run|run!=t.run)) {
                    return false;
                }
                self.starts.remove(id);
                let old = self.active.remove(id);
                let removed = old.is_some();
                let old=old.or_else(||{
                    let run=frame["run_id"].as_str().filter(|s|!s.is_empty())?;
                    let pos=self.retired.iter().position(|(known,t)|known==id && t.run==run)?;
                    self.retired.remove(pos).map(|(_,t)|t)
                });
                if frame["ambient"] == true { return removed; }
                let mut receipt = json!({"taskId":id,"runId":frame["run_id"],"toolUseId":frame["tool_use_id"],
                    "status":frame["status"],"summary":frame["summary"]});
                if self.finished.iter().any(|known| known["taskId"] == id && known["status"] == receipt["status"]
                    && known["summary"] == receipt["summary"] && known["toolUseId"] == receipt["toolUseId"]) { return removed; }
                receipt["completedAtMs"] = json!(now);
                if let Some(task) = old {
                    receipt["confirmedBackground"] = json!(true);
                    receipt["outputPending"]=json!(task.kind=="local_bash" && !task.run.is_empty());
                    receipt["runId"]=json!(task.run);
                    receipt["name"] = json!(task.name); receipt["kind"] = json!(task.kind);
                    receipt["startedAtMs"] = json!(task.started_at_ms);
                }
                self.finished.retain(|known| known["taskId"] != id);
                if self.finished.len() == 64 { self.finished.pop_front(); }
                self.finished.push_back(receipt);
                true
            }
            _ => false,
        }
    }

    pub(super) fn receipts(&self) -> &VecDeque<Value> { &self.finished }

    pub(super) fn items(&self) -> Value {
        let mut tasks: Vec<_> = self.active.iter().collect();
        tasks.sort_by_key(|(_, task)| task.order);
        json!(
            tasks
                .into_iter()
                .map(|(id, task)| json!({
                    "id":id,"name":task.name,"kind":task.kind,"status":"running","runId":task.run,"toolUseId":task.tool,"stopRequested":task.stop_requested,
                    "startedAtMs":task.started_at_ms,"elapsedMs":task.elapsed_ms,
                    "sampledAtMs":task.sampled_at_ms
                }))
                .collect::<Vec<_>>()
        )
    }

    fn trim_starts(&mut self) {
        // 仅用于保留可能稍后转入后台的前台开始事件；缺少结束通知时也不能无限积累。
        while self.starts.len() > 512 {
            let oldest = self
                .starts
                .iter()
                .filter(|(id, _)| !self.active.contains_key(*id))
                .min_by_key(|(_, task)| task.order)
                .map(|(id, _)| id.clone());
            let Some(id) = oldest else { break };
            self.starts.remove(&id);
        }
    }

    pub(super) fn summary(&self) -> String {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for task in self.active.values() {
            let name = match task.kind.as_str() {
                kind if kind.contains("bash") || kind.contains("shell") => "shell",
                kind if kind.contains("monitor") => "monitor",
                kind if kind.contains("agent") => "agent",
                _ => "task",
            };
            if let Some((_, count)) = counts.iter_mut().find(|(known, _)| *known == name) {
                *count += 1;
            } else {
                counts.push((name, 1));
            }
        }
        counts
            .into_iter()
            .map(|(name, count)| {
                format!(
                    "{count} {name}{}",
                    if count > 1 && name != "monitor" {
                        "s"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn task_id(value: &Value) -> Option<&str> {
    value["task_id"].as_str().filter(|id| !id.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_task_retired_before_notice_cannot_reappear_or_stop_a_new_run() {
        let mut tasks=BackgroundTasks::default();
        let started=json!({"subtype":"task_started","task_type":"local_bash","task_id":"A","run_id":"run1","is_backgrounded":true});
        tasks.observe(&started,1);assert!(tasks.stop_marker("A","run1",true));assert!(!tasks.stop_marker("A","run1",true));
        tasks.observe(&json!({"subtype":"background_tasks_changed","tasks":[]}),2);
        let ended=json!({"subtype":"task_notification","task_id":"A","run_id":"run1","status":"stopped","summary":"原文"});
        tasks.observe(&ended,3);assert!(tasks.finished_shell("A","run1"));
        tasks.observe(&started,4);assert!(!tasks.has_tasks());
        let mut next=started.clone();next["run_id"]=json!("run2");tasks.observe(&next,5);
        tasks.observe(&ended,6);assert!(tasks.can_control("A","run2"));assert!(!tasks.can_control("A","run1"));
    }

    #[test]
    fn foreground_work_enters_the_list_only_after_background_confirmation() {
        let mut tasks = BackgroundTasks::default();
        let started = json!({"subtype":"task_started","task_id":"a","task_type":"local_bash",
            "description":"等待结束","is_backgrounded":false,"_elvesTaskStartedAtMs":1000});
        tasks.observe(&started, 1000);
        assert!(!tasks.has_tasks());
        tasks.observe(
            &json!({"subtype":"background_tasks_changed","tasks":[
            {"task_id":"a","task_type":"local_bash","description":"等待结束"}]}),
            4000,
        );
        assert_eq!(tasks.items()[0]["startedAtMs"], 1000);
        assert_eq!(tasks.items()[0]["name"], "等待结束");
        assert_eq!(tasks.summary(), "1 shell");
    }

    #[test]
    fn snapshots_and_replayed_starts_preserve_time_and_finish_removes_the_task() {
        let mut tasks = BackgroundTasks::default();
        let started = json!({"subtype":"task_started","task_id":"a","task_type":"local_bash",
            "description":"测试命令","_elvesTaskStartedAtMs":1000});
        tasks.observe(&started, 1000);
        tasks.observe(&started, 10_000);
        tasks.observe(
            &json!({"subtype":"background_tasks_changed","tasks":[{"task_id":"a"}]}),
            11_000,
        );
        assert_eq!(tasks.items()[0]["startedAtMs"], 1000);
        assert_eq!(tasks.items()[0]["name"], "测试命令");
        tasks.observe(
            &json!({"subtype":"task_notification","task_id":"a","status":"running"}),
            12_000,
        );
        assert!(tasks.has_tasks());
        tasks.observe(
            &json!({"subtype":"task_notification","task_id":"a","status":"completed"}),
            13_000,
        );
        assert!(!tasks.has_tasks());
    }

    #[test]
    fn registry_without_start_evidence_does_not_invent_execution_time() {
        let mut tasks = BackgroundTasks::default();
        tasks.observe(
            &json!({"subtype":"background_tasks_changed","tasks":[
            {"task_id":"a","description":"仍在执行"},{"task_id":"hidden","ambient":true}]}),
            5000,
        );
        assert_eq!(tasks.items().as_array().unwrap().len(), 1);
        assert!(tasks.items()[0]["startedAtMs"].is_null());
        assert!(tasks.items()[0]["elapsedMs"].is_null());
        tasks.observe(&json!({"subtype":"background_tasks_changed"}), 6000);
        assert!(tasks.has_tasks(), "损坏的状态帧不能冒充确认没有任务");
        tasks.observe(
            &json!({"subtype":"background_tasks_changed","tasks":[]}),
            7000,
        );
        assert!(!tasks.has_tasks());
    }

    #[test]
    fn reattached_registry_keeps_the_hosts_original_start_time() {
        let mut tasks = BackgroundTasks::default();
        tasks.observe(
            &json!({"subtype":"background_tasks_changed","tasks":[
            {"task_id":"a","task_type":"local_bash","description":"已在后台运行",
                "_elvesTaskStartedAtMs":1000}]}),
            20_000,
        );
        assert_eq!(tasks.items()[0]["startedAtMs"], 1000);
        tasks.observe(
            &json!({"subtype":"background_tasks_changed","tasks":[{"task_id":"a"}]}),
            30_000,
        );
        assert_eq!(tasks.items()[0]["startedAtMs"], 1000);
    }

    #[test]
    fn native_progress_provides_elapsed_time_without_turning_foreground_work_into_background() {
        let mut tasks = BackgroundTasks::default();
        tasks.observe(
            &json!({"subtype":"task_progress","task_id":"foreground","usage":{"duration_ms":10}}),
            1000,
        );
        assert!(!tasks.has_tasks());
        tasks.observe(
            &json!({"subtype":"background_tasks_changed","tasks":[{"task_id":"a"}]}),
            1000,
        );
        tasks.observe(
            &json!({"subtype":"task_progress","task_id":"a","usage":{"duration_ms":2500}}),
            5000,
        );
        assert_eq!(tasks.items()[0]["elapsedMs"], 2500);
        assert_eq!(tasks.items()[0]["sampledAtMs"], 5000);
        tasks.clear();
        assert!(!tasks.has_tasks());
    }
}
