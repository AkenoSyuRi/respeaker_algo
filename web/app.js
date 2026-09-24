(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    config: null,
    snapshot: null,
    latestDoa: null,
    latestBf: null,
    dirty: false,
    rawEditorDirty: false,
    source: null,
    log: [],
    reconnectTimer: null,
    retryDelay: 500,
    recordings: [],
    trash: [],
  };
  const modeNames = { pure: "原始录音", doa: "声源定位", doa_bf: "自动拾音", bf_fixed: "定向拾音", custom: "自定义" };
  const fileNames = { algo: "设备处理音频", mic: "原始 4-Mic", ref: "Playback / AEC Reference", bf: "Beamformer 输出", doa_csv: "DOA 数据", manifest: "录音信息" };

  function clone(value) {
    return typeof structuredClone === "function" ? structuredClone(value) : JSON.parse(JSON.stringify(value));
  }

  function request(path, options) {
    options = options || {};
    const headers = Object.assign({ "Content-Type": "application/json" }, options.headers || {});
    return fetch(path, Object.assign({}, options, { headers })).then(async (response) => {
      if (!response.ok) {
        let detail = "HTTP " + response.status;
        try {
          const body = await response.json();
          detail = body.error?.message || detail;
        } catch (_) { /* Non-JSON error response. */ }
        throw new Error(detail);
      }
      return response.status === 204 ? null : response.json();
    });
  }

  function getModules(config) {
    return Array.isArray(config?.pipeline?.modules) ? config.pipeline.modules : [];
  }

  function getModule(config, type) {
    return getModules(config).find((module) => module.type === type) || null;
  }

  function detectMode(config) {
    if (!config?.pipeline_enabled) return "pure";
    const doa = getModule(config, "doa");
    const bf = getModule(config, "beamformer");
    const doaEnabled = Boolean(doa?.enabled);
    const bfEnabled = Boolean(bf?.enabled);
    if (doaEnabled && !bfEnabled) return "doa";
    if (doaEnabled && bfEnabled && bf.direction_source === "doa") return "doa_bf";
    if (!doaEnabled && bfEnabled && bf.direction_source === "fixed") return "bf_fixed";
    return "custom";
  }

  function setFeedback(text, error) {
    for (const id of ["config-message", "advanced-message"]) {
      const element = $(id);
      element.textContent = text || "";
      element.classList.toggle("error", Boolean(error));
      element.classList.toggle("success", Boolean(text) && !error);
    }
  }

  function setDirty(dirty) {
    state.dirty = Boolean(dirty);
    const element = $("dirty-status");
    element.textContent = state.dirty ? "有未保存更改" : "设置已保存";
    element.classList.toggle("dirty", state.dirty);
  }

  function selectedRadio(name, fallback) {
    return document.querySelector('input[name="' + name + '"]:checked')?.value ?? fallback;
  }

  function setRadio(name, value) {
    for (const input of document.querySelectorAll('input[name="' + name + '"]')) {
      input.checked = input.value === String(value);
    }
  }

  function renderMode(config) {
    const mode = detectMode(config);
    $("current-mode").textContent = "当前模式：" + modeNames[mode];
    $("custom-mode-note").hidden = mode !== "custom";
    for (const button of document.querySelectorAll("[data-mode]")) {
      button.setAttribute("aria-pressed", String(button.dataset.mode === mode));
    }
  }

  function renderConfig(config) {
    state.config = config;
    const recording = config.recording || {};
    const doa = getModule(config, "doa") || {};
    const bf = getModule(config, "beamformer") || {};

    $("config-editor").value = JSON.stringify(config, null, 2);
    state.rawEditorDirty = false;
    $("out-dir").value = recording.out_dir ?? "recordings";
    $("recording-prefix").value = recording.prefix ?? "";
    const duration = Number(recording.duration_seconds || 0);
    $("duration-manual").checked = duration === 0;
    $("duration-custom").checked = duration > 0;
    $("duration-seconds").value = duration > 0 ? duration : 300;

    $("doa-angle-offset").value = doa.angle_offset_deg ?? 0;
    setRadio("doa-clockwise", Boolean(doa.clockwise));
    $("doa-beta").value = doa.beta ?? 0.75;
    $("doa-cpsd-tau").value = doa.cpsd_tau_ms ?? 100;
    $("doa-acquire-confidence").value = doa.acquire_confidence ?? 0.55;
    $("doa-update-confidence").value = doa.update_confidence ?? 0.35;
    $("doa-max-coast").value = doa.max_coast_ms ?? 500;
    $("doa-csv").checked = Boolean(doa.csv);

    setRadio("bf-direction-source", bf.direction_source ?? "doa");
    $("bf-fixed-angle").value = bf.fixed_internal_angle_deg ?? 0;
    $("bf-fallback-angle").value = bf.fallback_internal_angle_deg ?? 0;
    $("bf-output-gain").value = bf.output_gain_db ?? -3;
    $("bf-enable-drc").checked = bf.enable_drc !== false;
    $("bf-algorithm").value = bf.algorithm ?? "robust_superdirective";
    $("bf-smoothing").value = bf.direction_smoothing_ms ?? 64;
    $("bf-min-wng").value = bf.min_wng_db ?? 3;
    $("bf-sd-low-start").value = bf.sd_low_start_hz ?? 350;
    $("bf-sd-low-full").value = bf.sd_low_full_hz ?? 500;
    $("bf-sd-high-full").value = bf.sd_high_full_hz ?? 2500;
    $("bf-sd-high-end").value = bf.sd_high_end_hz ?? 3500;
    $("bf-wav").checked = bf.wav !== false;
    $("bf-compare-wav").checked = Boolean(bf.compare_wav);

    renderMode(config);
    updateConditionalUi();
    renderLivePanel();
    renderStatusBanner();
    $("recordings-location").textContent = "文件位置：" + (recording.out_dir || "recordings");
    $("live-save-location").textContent = "保存至 " + (recording.out_dir || "recordings");
  }

  function moduleEnabled(config, type) {
    return Boolean(config?.pipeline_enabled && getModule(config, type)?.enabled);
  }

  function updateConditionalUi() {
    $("duration-value-wrap").hidden = !$("duration-custom").checked;
    if (!state.config) return;
    const doaEnabled = moduleEnabled(state.config, "doa");
    const bfEnabled = moduleEnabled(state.config, "beamformer");
    $("doa-inactive").hidden = doaEnabled;
    $("doa-controls").hidden = !doaEnabled;
    $("bf-inactive").hidden = bfEnabled;
    $("bf-controls").hidden = !bfEnabled;
    $("doa-module-state").textContent = doaEnabled ? "当前使用" : "未使用";
    $("bf-module-state").textContent = bfEnabled ? "当前使用" : "未使用";
    $("doa-module-state").classList.toggle("active", doaEnabled);
    $("bf-module-state").classList.toggle("active", bfEnabled);
    const direction = selectedRadio("bf-direction-source", getModule(state.config, "beamformer")?.direction_source || "doa");
    $("bf-fallback-setting").hidden = !bfEnabled || direction !== "doa";
    $("bf-fixed-setting").hidden = !bfEnabled || direction !== "fixed";
    const delaySum = $("bf-algorithm").value === "delay_sum";
    $("bf-smoothing-setting").hidden = !bfEnabled || direction === "fixed";
    $("bf-min-wng-setting").hidden = !bfEnabled || delaySum;
    $("bf-frequency-setting").hidden = !bfEnabled || delaySum;
    $("bf-compare-wav-setting").hidden = !$("bf-wav").checked;
    renderLivePanel();
  }

  function numberFrom(id) {
    const value = Number($(id).value);
    return Number.isFinite(value) ? value : 0;
  }

  function updateModule(config, type, values) {
    const modules = config.pipeline.modules;
    const index = modules.findIndex((module) => module.type === type);
    if (index >= 0) modules[index] = Object.assign({}, modules[index], values);
    else modules.push(Object.assign({ type: type }, values));
  }

  function collectConfigFromUi() {
    if (!state.config) throw new Error("配置尚未加载");
    const config = clone(state.config);
    config.recording = Object.assign({}, config.recording || {});
    config.recording.out_dir = $("out-dir").value;
    config.recording.prefix = $("recording-prefix").value.trim() || null;
    config.recording.duration_seconds = $("duration-custom").checked ? Math.max(1, Math.round(numberFrom("duration-seconds"))) : 0;
    config.pipeline = Object.assign({ version: 1, modules: [] }, config.pipeline || {});
    config.pipeline.modules = getModules(config).map((module) => Object.assign({}, module));

    if (moduleEnabled(state.config, "doa")) {
      updateModule(config, "doa", {
        angle_offset_deg: numberFrom("doa-angle-offset"),
        clockwise: selectedRadio("doa-clockwise", "false") === "true",
        beta: numberFrom("doa-beta"),
        cpsd_tau_ms: numberFrom("doa-cpsd-tau"),
        acquire_confidence: numberFrom("doa-acquire-confidence"),
        update_confidence: numberFrom("doa-update-confidence"),
        max_coast_ms: Math.max(1, Math.round(numberFrom("doa-max-coast"))),
        csv: $("doa-csv").checked,
      });
    }

    if (moduleEnabled(state.config, "beamformer")) {
      const direction = selectedRadio("bf-direction-source", "doa");
      updateModule(config, "beamformer", {
        direction_source: direction,
        fixed_internal_angle_deg: numberFrom("bf-fixed-angle"),
        fallback_internal_angle_deg: numberFrom("bf-fallback-angle"),
        output_gain_db: numberFrom("bf-output-gain"),
        enable_drc: $("bf-enable-drc").checked,
        algorithm: $("bf-algorithm").value,
        direction_smoothing_ms: numberFrom("bf-smoothing"),
        min_wng_db: numberFrom("bf-min-wng"),
        sd_low_start_hz: numberFrom("bf-sd-low-start"),
        sd_low_full_hz: numberFrom("bf-sd-low-full"),
        sd_high_full_hz: numberFrom("bf-sd-high-full"),
        sd_high_end_hz: numberFrom("bf-sd-high-end"),
        wav: $("bf-wav").checked,
        compare_wav: $("bf-compare-wav").checked,
      });
    }

    const modules = config.pipeline.modules;
    const doaIndex = modules.findIndex((module) => module.type === "doa" && module.enabled);
    const bfIndex = modules.findIndex((module) => module.type === "beamformer" && module.enabled);
    const bf = modules[bfIndex];
    if (doaIndex >= 0 && bfIndex >= 0 && bf?.direction_source === "doa" && doaIndex > bfIndex) {
      const [doa] = modules.splice(doaIndex, 1);
      modules.splice(bfIndex, 0, doa);
    }
    return config;
  }

  async function commitConfig(persist) {
    const config = collectConfigFromUi();
    const response = await request("/api/config", { method: "PUT", body: JSON.stringify(config) });
    renderConfig(response.draft);
    if (persist) {
      await request("/api/config/save", { method: "POST" });
      setDirty(false);
      setFeedback("设置已保存。", false);
    } else {
      setDirty(state.dirty);
    }
    return response.draft;
  }

  async function selectBuiltInMode(name) {
    const recording = collectConfigFromUi().recording;
    const response = await request("/api/profiles/" + encodeURIComponent(name));
    const config = response.draft;
    config.recording = recording;
    const updated = await request("/api/config", { method: "PUT", body: JSON.stringify(config) });
    renderConfig(updated.draft);
    setDirty(true);
    setFeedback("已切换到“" + modeNames[name] + "”。录音设置已保留。", false);
  }

  function phaseInfo(snapshot) {
    const phase = snapshot?.phase || "idle";
    const phaseName = (typeof phase === "string" ? phase : Object.keys(phase)[0] || "idle").toLowerCase();
    const details = typeof phase === "object" ? phase[phaseName] : null;
    return { name: phaseName, sessionId: details?.session_id || null };
  }

  function formatDuration(seconds) {
    const total = Math.max(0, Math.floor(Number(seconds) || 0));
    const hours = Math.floor(total / 3600);
    const minutes = Math.floor((total % 3600) / 60);
    const remainder = total % 60;
    const pad = (value) => String(value).padStart(2, "0");
    return hours ? pad(hours) + ":" + pad(minutes) + ":" + pad(remainder) : pad(minutes) + ":" + pad(remainder);
  }

  function setConnection(connected) {
    $("connection-status").classList.toggle("connected", connected);
    $("connection-status").classList.toggle("disconnected", !connected);
    $("connection-text").textContent = connected ? "实时连接" : "连接断开";
  }

  function renderSnapshot(snapshot) {
    snapshot = snapshot?.payload || snapshot;
    if (!snapshot) return;
    const previousPhase = phaseInfo(state.snapshot).name;
    state.snapshot = Object.assign({}, state.snapshot || {}, snapshot);
    const current = state.snapshot;
    const phase = phaseInfo(current);
    const phaseName = phase.name;
    if (phaseName === "starting" && previousPhase !== "starting") {
      state.latestDoa = null;
      state.latestBf = null;
      current.recording = Object.assign({}, current.recording || {}, { captured_frames: 0, elapsed_secs: 0 });
      if (current.pipeline) {
        current.pipeline = Object.assign({}, current.pipeline);
        delete current.pipeline.bf_stats;
      }
      $("doa-angle").textContent = "--°";
      $("doa-status").textContent = "算法正在启动…";
      $("doa-needle").style.transform = "translate(-50%,-100%) rotate(180deg)";
    }
    const phaseLabels = { idle: "等待开始录音", starting: "正在启动…", recording: "录音中", stopping: "正在停止并保存文件…" };
    $("phase-label").textContent = phaseLabels[phaseName] || phaseName;
    $("recording-state").classList.toggle("active", phaseName === "recording");
    $("start-button").disabled = phaseName !== "idle" || !current.device?.available;
    $("stop-button").disabled = phaseName === "idle" || phaseName === "stopping";
    $("session-note").hidden = phaseName === "idle";
    $("elapsed-time").textContent = formatDuration(current.recording?.elapsed_secs);
    const device = current.device || {};
    $("device-status").classList.toggle("connected", Boolean(device.available));
    $("device-status").classList.toggle("unavailable", !device.available);
    $("device-status").classList.remove("pending");
    $("device-status-text").textContent = device.available ? "ReSpeaker 已连接" : "未检测到 ReSpeaker";
    renderStatusBanner();
    renderDetailedInfo();
    renderLivePanel();
  }

  function renderStatusBanner() {
    const snapshot = state.snapshot;
    let text = "";
    let warning = false;
    const device = snapshot?.device;
    const error = snapshot?.last_error;
    const pipeline = snapshot?.pipeline;
    const source = String(error?.source || "").toLowerCase();
    const clipped = phaseInfo(snapshot).name === "recording"
      ? Number(pipeline?.bf_stats?.clipped_samples || state.latestBf?.clipped_samples || 0)
      : 0;
    if (device && !device.available) {
      text = "未检测到 ReSpeaker。" + (device.error ? " " + device.error : "");
    } else if (error && source !== "pipeline") {
      text = "录音错误：" + error.message;
    } else if (pipeline?.degraded) {
      text = "算法处理已降级。" + (pipeline.error || error?.message || "");
      warning = true;
    } else if (snapshot?.config_warning) {
      text = snapshot.config_warning;
      warning = true;
    } else if (clipped > 0) {
      text = "检测到 Beamformer 输出削波。";
      warning = true;
    }
    const banner = $("status-banner");
    banner.hidden = !text;
    banner.classList.toggle("warning", warning);
    $("status-banner-message").textContent = text;
  }

  function doaStatusLabel(status) {
    return ({ tracking: "方向稳定", searching: "正在搜索声源", coasting: "暂时保持上一方向" })[status] || "正在搜索声源";
  }

  function renderDoa(data) {
    data = data?.payload || data;
    state.latestDoa = data;
    const tracked = Number(data.tracked_angle_deg);
    const raw = Number(data.raw_angle_deg);
    $("doa-angle").textContent = Number.isFinite(tracked) ? tracked.toFixed(0) + "°" : "--°";
    $("doa-status").textContent = doaStatusLabel(data.status);
    $("doa-needle").style.transform = "translate(-50%,-100%) rotate(" + (180 + (Number.isFinite(tracked) ? tracked : 0)) + "deg)";
    $("detail-tracked-angle").textContent = Number.isFinite(tracked) ? tracked.toFixed(1) + "°" : "--";
    $("detail-raw-angle").textContent = Number.isFinite(raw) ? raw.toFixed(1) + "°" : "--";
    $("detail-confidence").textContent = Number.isFinite(Number(data.confidence)) ? Number(data.confidence).toFixed(2) : "--";
    $("detail-doa-status").textContent = doaStatusLabel(data.status);
    renderLivePanel();
  }

  function renderBfStats(data) {
    data = data?.payload || data;
    state.latestBf = data;
    if (state.snapshot?.pipeline) state.snapshot.pipeline.bf_stats = data;
    renderStatusBanner();
    renderDetailedInfo();
    renderLivePanel();
  }

  function renderLivePanel() {
    if (!state.config) return;
    const mode = detectMode(state.config);
    const doa = getModule(state.config, "doa");
    const bf = getModule(state.config, "beamformer");
    const doaEnabled = moduleEnabled(state.config, "doa");
    const bfEnabled = moduleEnabled(state.config, "beamformer");
    const direction = selectedRadio("bf-direction-source", bf?.direction_source || "doa");
    const showDoa = mode === "doa" || mode === "doa_bf" || (mode === "custom" && doaEnabled);
    const showFixed = mode === "bf_fixed" || (mode === "custom" && bfEnabled && direction === "fixed" && !doaEnabled);
    const showPure = !showDoa && !showFixed;
    $("live-pure").hidden = !showPure;
    $("live-doa").hidden = !showDoa;
    $("live-fixed").hidden = !showFixed;
    $("live-hint").hidden = showPure;
    $("live-save-location").textContent = "保存至 " + ($("out-dir").value.trim() || "recordings");
    $("live-subtitle").textContent = showPure ? "当前模式保留原始多通道音频。" : showFixed ? "固定方向以 Beamformer 内部角度显示。" : "实时显示声源方向与算法状态。";
    if (showFixed) {
      const inputAngle = Number($("bf-fixed-angle").value);
      const angle = Number.isFinite(inputAngle) ? ((inputAngle % 360) + 360) % 360 : Number(bf?.fixed_internal_angle_deg || 0);
      $("fixed-angle").textContent = angle.toFixed(0) + "°";
      $("fixed-needle").style.transform = "translate(-50%,-100%) rotate(" + (90 - angle) + "deg)";
    }
    const pipeline = state.snapshot?.pipeline || {};
    const phase = phaseInfo(state.snapshot).name;
    const recording = phase === "recording";
    const status = $("algorithm-status");
    status.hidden = !bfEnabled;
    status.classList.toggle("degraded", recording && Boolean(pipeline.degraded));
    if (!recording) {
      status.textContent = ({ idle: "录音开始后启用", starting: "算法正在启动…", stopping: "正在停止…" })[phase] || "录音开始后启用";
    } else {
      status.textContent = pipeline.degraded
        ? "算法处理已降级：" + (pipeline.error || "请查看详细信息。")
        : direction === "fixed" ? "定向拾音正常" : "自动拾音正常";
    }
    const stats = recording ? (pipeline.bf_stats || state.latestBf || {}) : {};
    $("clipping-warning").hidden = !recording || Number(stats.clipped_samples || 0) === 0;
  }

  function renderDetailedInfo() {
    const snapshot = state.snapshot || {};
    const recording = snapshot.recording || {};
    const pipeline = snapshot.pipeline || {};
    const doa = state.latestDoa || {};
    const bf = pipeline.bf_stats || state.latestBf || {};
    $("detail-tracked-angle").textContent = Number.isFinite(Number(doa.tracked_angle_deg)) ? Number(doa.tracked_angle_deg).toFixed(1) + "°" : "--";
    $("detail-raw-angle").textContent = Number.isFinite(Number(doa.raw_angle_deg)) ? Number(doa.raw_angle_deg).toFixed(1) + "°" : "--";
    $("detail-confidence").textContent = Number.isFinite(Number(doa.confidence)) ? Number(doa.confidence).toFixed(2) : "--";
    $("detail-doa-status").textContent = doa.status ? doaStatusLabel(doa.status) : "--";
    $("detail-captured-frames").textContent = Number(recording.captured_frames || 0).toLocaleString();
    $("detail-bf-frames").textContent = Number(bf.output_frames || 0).toLocaleString();
    $("detail-clipped").textContent = Number(bf.clipped_samples || 0).toLocaleString();
    $("detail-pipeline").textContent = pipeline.degraded ? "已降级" : pipeline.enabled ? "正常" : "未启用";
  }

  function addLog(type, data) {
    state.log.unshift("[" + new Date().toLocaleTimeString() + "] " + type + " " + JSON.stringify(data));
    state.log = state.log.slice(0, 80);
    $("event-log").textContent = state.log.join("\n");
  }

  function connectEvents() {
    const source = new EventSource("/api/events");
    state.source = source;
    source.onopen = () => {
      setConnection(true);
      state.retryDelay = 500;
      request("/api/status").then(renderSnapshot).catch(() => {});
    };
    source.onerror = () => {
      setConnection(false);
      source.close();
      if (!state.reconnectTimer) {
        const delay = state.retryDelay;
        state.retryDelay = Math.min(state.retryDelay * 2, 10000);
        state.reconnectTimer = window.setTimeout(() => { state.reconnectTimer = null; connectEvents(); }, delay);
      }
    };
    const eventTypes = ["state_snapshot", "device_status", "recording_progress", "pipeline_status", "doa", "bf_stats", "error", "session_finished"];
    for (const type of eventTypes) {
      source.addEventListener(type, (event) => {
        let message;
        try { message = JSON.parse(event.data); } catch (_) { message = event.data; }
        addLog(type, message);
        const payload = message?.payload || message;
        if (type === "state_snapshot") renderSnapshot(payload);
        if (type === "device_status" && state.snapshot) { state.snapshot.device = payload; renderSnapshot(state.snapshot); }
        if (type === "recording_progress" && state.snapshot) {
          state.snapshot.recording = Object.assign({}, state.snapshot.recording || {}, payload);
          renderSnapshot(state.snapshot);
        }
        if (type === "pipeline_status" && state.snapshot) { state.snapshot.pipeline = Object.assign({}, state.snapshot.pipeline || {}, payload); renderSnapshot(state.snapshot); }
        if (type === "doa") renderDoa(payload);
        if (type === "bf_stats") renderBfStats(payload);
        if (type === "error" && state.snapshot) { state.snapshot.last_error = payload; renderStatusBanner(); }
        if (type === "session_finished") loadRecordings().catch((error) => setFeedback(error.message, true));
      });
    }
  }

  async function loadConfig() {
    const response = await request("/api/config");
    renderConfig(response.draft);
    if (response.config_warning && state.snapshot) state.snapshot.config_warning = response.config_warning;
    renderStatusBanner();
    setDirty(false);
  }

  async function loadProfiles() {
    const response = await request("/api/profiles");
    const select = $("profile-select");
    select.replaceChildren();
    const empty = document.createElement("option");
    empty.value = "";
    empty.textContent = response.profiles?.length ? "选择方案" : "没有保存的方案";
    select.append(empty);
    for (const name of response.profiles || []) {
      const option = document.createElement("option");
      option.value = name;
      option.textContent = name;
      select.append(option);
    }
    $("load-profile-button").disabled = !response.profiles?.length;
    $("delete-profile-button").disabled = !response.profiles?.length;
  }

  function statusLabel(status) {
    return ({ starting: "正在启动", recording: "录音中", completed: "已完成", failed: "失败", interrupted: "中断", legacy: "历史文件" })[String(status).toLowerCase()] || status || "未知";
  }

  function dateLabel(value) {
    if (!value) return "历史文件";
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" });
  }

  function createFileLink(recording, file) {
    const link = document.createElement("a");
    link.href = "/api/recordings/" + encodeURIComponent(recording.id) + "/files/" + encodeURIComponent(file.kind);
    link.download = "";
    link.textContent = fileNames[file.kind] || file.kind;
    return link;
  }

  function renderRecording(recording, trashed) {
    const row = document.createElement("article");
    row.className = "recording-row";
    const main = document.createElement("div");
    main.className = "recording-primary";
    const title = document.createElement("strong");
    title.textContent = recording.prefix || "录音";
    title.title = recording.prefix || "";
    const date = document.createElement("span");
    date.textContent = dateLabel(recording.manifest?.started_at);
    main.append(title, date);
    const mode = document.createElement("span");
    mode.className = "recording-mode";
    mode.textContent = recording.manifest?.active_config ? modeNames[detectMode(recording.manifest.active_config)] : "历史录音";
    const duration = document.createElement("span");
    duration.className = "recording-duration";
    const frames = Number(recording.manifest?.captured_frames || 0);
    duration.textContent = frames ? formatDuration(frames / 16000) : "—";
    row.append(main, mode, duration);

    const playable = ["bf", "algo", "ref"].find((kind) => recording.files?.some((file) => file.kind === kind && file.exists));
    if (playable && !trashed) {
      const player = document.createElement("audio");
      player.className = "recording-audio";
      player.controls = true;
      player.preload = "none";
      player.src = "/api/recordings/" + encodeURIComponent(recording.id) + "/files/" + playable;
      row.append(player);
    }
    const links = document.createElement("div");
    links.className = "file-links";
    if (!trashed) for (const file of recording.files || []) if (file.exists) links.append(createFileLink(recording, file));
    if (links.childNodes.length) row.append(links);

    const more = document.createElement("details");
    more.className = "recording-more";
    const summary = document.createElement("summary");
    summary.textContent = "更多";
    const action = document.createElement("button");
    action.type = "button";
    action.textContent = trashed ? "恢复" : "移入回收站";
    action.addEventListener("click", async () => {
      try {
        const operation = trashed ? "restore" : "trash";
        await request("/api/recordings/" + encodeURIComponent(recording.id) + "/" + operation, { method: "POST" });
        await loadRecordings();
      } catch (error) { setFeedback(error.message, true); }
    });
    more.append(summary, action);
    row.append(more);
    const stateLabel = document.createElement("span");
    stateLabel.className = "recording-mode";
    stateLabel.textContent = statusLabel(recording.status);
    stateLabel.hidden = !["failed", "interrupted", "starting", "recording"].includes(String(recording.status).toLowerCase());
    if (!stateLabel.hidden) row.insertBefore(stateLabel, duration);
    return row;
  }

  function renderRecordingList(target, entries, trashed) {
    target.replaceChildren();
    if (!entries.length) {
      const empty = document.createElement("p");
      empty.className = "empty-state";
      empty.textContent = trashed ? "回收站为空。" : "还没有录音记录。";
      target.append(empty);
      return;
    }
    for (const entry of entries) target.append(renderRecording(entry, trashed));
  }

  async function loadRecordings() {
    const response = await request("/api/recordings");
    state.recordings = response.recordings || [];
    state.trash = response.trash || [];
    $("recording-count").textContent = state.recordings.length ? "(" + state.recordings.length + ")" : "";
    $("trash-count").textContent = state.trash.length ? "(" + state.trash.length + ")" : "";
    renderRecordingList($("recordings-list"), state.recordings, false);
    renderRecordingList($("trash-list"), state.trash, true);
  }

  function showRecordingList(trash) {
    $("recordings-list").hidden = trash;
    $("trash-list").hidden = !trash;
    $("show-recordings-button").classList.toggle("selected", !trash);
    $("show-recordings-button").setAttribute("aria-pressed", String(!trash));
    $("show-trash-button").classList.toggle("selected", trash);
    $("show-trash-button").setAttribute("aria-pressed", String(trash));
  }

  async function saveSettings() {
    try { await commitConfig(true); } catch (error) { setFeedback(error.message, true); }
  }

  async function exportConfig() {
    try {
      await commitConfig(false);
      window.location.href = "/api/config/export";
    } catch (error) { setFeedback(error.message, true); }
  }

  async function startRecording() {
    $("start-button").disabled = true;
    try {
      await commitConfig(false);
      $("start-button").disabled = true;
      await request("/api/recordings/start", { method: "POST" });
      setFeedback("正在启动录音…", false);
    } catch (error) {
      setFeedback(error.message, true);
      renderSnapshot(state.snapshot);
    }
  }

  async function stopRecording() {
    try {
      await request("/api/recordings/stop", { method: "POST" });
      setFeedback("正在停止并保存文件…", false);
    } catch (error) { setFeedback(error.message, true); }
  }

  async function applyRawConfig() {
    try {
      const config = JSON.parse($("config-editor").value);
      const response = await request("/api/config", { method: "PUT", body: JSON.stringify(config) });
      renderConfig(response.draft);
      setDirty(true);
      setFeedback("原始配置已应用到当前设置。", false);
    } catch (error) { setFeedback(error.message, true); }
  }

  async function importConfig(event) {
    const file = event.target.files?.[0];
    if (!file) return;
    try {
      const response = await request("/api/config/import", { method: "POST", headers: { "Content-Type": "text/plain" }, body: await file.text() });
      renderConfig(response.draft);
      setDirty(true);
      setFeedback("TOML 配置已导入。", false);
    } catch (error) { setFeedback(error.message, true); }
    event.target.value = "";
  }

  async function loadProfile() {
    const name = $("profile-select").value;
    if (!name) return;
    try {
      const response = await request("/api/profiles/" + encodeURIComponent(name));
      renderConfig(response.draft);
      setDirty(true);
      setFeedback("方案“" + name + "”已载入。", false);
    } catch (error) { setFeedback(error.message, true); }
  }

  async function saveProfile() {
    const name = $("profile-name").value.trim();
    if (!name) { setFeedback("请输入方案名称。", true); return; }
    try {
      const config = collectConfigFromUi();
      await request("/api/profiles/" + encodeURIComponent(name), { method: "PUT", body: JSON.stringify(config) });
      await loadProfiles();
      $("profile-select").value = name;
      setFeedback("方案“" + name + "”已保存。", false);
    } catch (error) { setFeedback(error.message, true); }
  }

  async function deleteProfile() {
    const name = $("profile-select").value;
    if (!name || !window.confirm("确定删除方案“" + name + "”吗？")) return;
    try {
      await request("/api/profiles/" + encodeURIComponent(name), { method: "DELETE" });
      await loadProfiles();
      setFeedback("方案已删除。", false);
    } catch (error) { setFeedback(error.message, true); }
  }

  async function resetConfig() {
    try {
      const response = await request("/api/config/reset", { method: "POST" });
      renderConfig(response.draft);
      setDirty(true);
      $("reset-dialog").close();
      setFeedback("已恢复默认设置；保存设置后会写入配置文件。", false);
    } catch (error) { setFeedback(error.message, true); }
  }

  async function copyDiagnostics() {
    const snapshot = state.snapshot || {};
    const device = snapshot.device || {};
    const pipeline = snapshot.pipeline || {};
    const error = snapshot.last_error || {};
    const info = [
      "ReSpeaker Audio Manager",
      "Version: " + document.querySelector("#device-dialog .device-contract div:last-child dd")?.textContent,
      "Web connection: " + (state.source?.readyState === EventSource.OPEN ? "connected" : "disconnected"),
      "Device: " + (device.name || "ReSpeaker Mic Array v2.0"),
      "Device available: " + Boolean(device.available),
      "Phase: " + phaseInfo(snapshot).name,
      "Mode: " + modeNames[state.config ? detectMode(state.config) : "custom"],
      "Pipeline enabled: " + Boolean(pipeline.enabled),
      "Pipeline degraded: " + Boolean(pipeline.degraded),
      "Pipeline error: " + (pipeline.error || ""),
      "Last error source: " + (error.source || ""),
      "Last error code: " + (error.code || ""),
      "Last error message: " + (error.message || ""),
      "Config warning: " + (snapshot.config_warning || ""),
    ].join("\n");
    try {
      if (!navigator.clipboard?.writeText) throw new Error("浏览器不支持剪贴板访问。");
      await navigator.clipboard.writeText(info);
      setFeedback("诊断信息已复制。", false);
    } catch (error) { setFeedback("无法复制诊断信息：" + error.message, true); }
  }

  function updatePicker() {
    const angle = Number($("bf-fixed-angle").value);
    const normalized = Number.isFinite(angle) ? ((angle % 360) + 360) % 360 : 0;
    $("bf-picker-needle").style.transform = "translate(-50%,-100%) rotate(" + (90 - normalized) + "deg)";
    $("bf-direction-picker").setAttribute("aria-valuenow", normalized.toFixed(1));
  }

  function setPickerFromPointer(event) {
    const rect = $("bf-direction-picker").getBoundingClientRect();
    const dx = event.clientX - (rect.left + rect.width / 2);
    const dy = (rect.top + rect.height / 2) - event.clientY;
    // Picker geometry uses the BF internal angle: 0° is +X and 90° is +Y.
    const angle = (Math.atan2(dy, dx) * 180 / Math.PI + 360) % 360;
    $("bf-fixed-angle").value = angle.toFixed(1);
    updatePicker();
    setDirty(true);
  }

  function switchTab(name, focus) {
    const valid = ["run", "algorithm", "recordings", "advanced"];
    if (!valid.includes(name)) name = "run";
    for (const tab of document.querySelectorAll("[role=tab][data-tab]")) {
      const selected = tab.dataset.tab === name;
      tab.setAttribute("aria-selected", String(selected));
      tab.tabIndex = selected ? 0 : -1;
      $("panel-" + tab.dataset.tab).hidden = !selected;
      if (selected && focus) tab.focus();
    }
    if (name === "recordings") loadRecordings().catch((error) => setFeedback(error.message, true));
  }

  function openDialog(id) {
    const dialog = $(id);
    if (dialog && !dialog.open) dialog.showModal();
  }

  function bindEvents() {
    for (const tab of document.querySelectorAll("[role=tab][data-tab]")) tab.addEventListener("click", () => switchTab(tab.dataset.tab));
    $("tab-run").parentElement.addEventListener("keydown", (event) => {
      if (!["ArrowLeft", "ArrowRight"].includes(event.key)) return;
      const tabs = Array.from(document.querySelectorAll("[role=tab][data-tab]"));
      const current = tabs.indexOf(document.activeElement);
      const step = event.key === "ArrowRight" ? 1 : -1;
      const next = tabs[(current + step + tabs.length) % tabs.length];
      switchTab(next.dataset.tab, true);
      event.preventDefault();
    });

    for (const button of document.querySelectorAll("[data-mode], [data-preset]")) {
      button.addEventListener("click", () => {
        const name = button.dataset.mode || button.dataset.preset;
        selectBuiltInMode(name).catch((error) => setFeedback(error.message, true));
      });
    }

    for (const id of ["out-dir", "recording-prefix", "duration-seconds", "doa-angle-offset", "doa-beta", "doa-cpsd-tau", "doa-acquire-confidence", "doa-update-confidence", "doa-max-coast", "bf-fixed-angle", "bf-fallback-angle", "bf-output-gain", "bf-algorithm", "bf-smoothing", "bf-min-wng", "bf-sd-low-start", "bf-sd-low-full", "bf-sd-high-full", "bf-sd-high-end"]) {
      $(id).addEventListener("input", () => {
        setDirty(true);
        updateConditionalUi();
        updatePicker();
      });
      $(id).addEventListener("change", () => { setDirty(true); updateConditionalUi(); });
    }
    for (const selector of ['input[name="duration-mode"]', 'input[name="doa-clockwise"]', 'input[name="bf-direction-source"]', "#doa-csv", "#bf-enable-drc", "#bf-wav", "#bf-compare-wav"]) {
      for (const input of document.querySelectorAll(selector)) {
        input.addEventListener("change", () => { setDirty(true); updateConditionalUi(); });
      }
    }

    $("duration-manual").addEventListener("change", updateConditionalUi);
    $("duration-custom").addEventListener("change", updateConditionalUi);
    $("bf-fixed-angle").addEventListener("input", updatePicker);
    const picker = $("bf-direction-picker");
    picker.addEventListener("pointerdown", (event) => { picker.setPointerCapture(event.pointerId); setPickerFromPointer(event); });
    picker.addEventListener("pointermove", (event) => { if (picker.hasPointerCapture(event.pointerId)) setPickerFromPointer(event); });
    picker.addEventListener("keydown", (event) => {
      const current = Number($("bf-fixed-angle").value) || 0;
      if (event.key === "Home") $("bf-fixed-angle").value = "0";
      else if (event.key === "End") $("bf-fixed-angle").value = "359.9";
      else if (["ArrowLeft", "ArrowDown"].includes(event.key)) $("bf-fixed-angle").value = String((current + 359) % 360);
      else if (["ArrowRight", "ArrowUp"].includes(event.key)) $("bf-fixed-angle").value = String((current + 1) % 360);
      else return;
      event.preventDefault();
      updatePicker();
      setDirty(true);
    });

    $("start-button").addEventListener("click", startRecording);
    $("stop-button").addEventListener("click", stopRecording);
    $("save-settings-button").addEventListener("click", saveSettings);
    $("refresh-button").addEventListener("click", () => loadRecordings().catch((error) => setFeedback(error.message, true)));
    $("show-recordings-button").addEventListener("click", () => showRecordingList(false));
    $("show-trash-button").addEventListener("click", () => showRecordingList(true));
    $("load-profile-button").addEventListener("click", loadProfile);
    $("save-profile-button").addEventListener("click", saveProfile);
    $("delete-profile-button").addEventListener("click", deleteProfile);
    $("profile-select").addEventListener("change", () => { $("delete-profile-button").disabled = !$("profile-select").value; });
    $("import-button").addEventListener("click", () => $("import-file").click());
    $("import-file").addEventListener("change", importConfig);
    $("export-button").addEventListener("click", exportConfig);
    $("reset-button").addEventListener("click", () => openDialog("reset-dialog"));
    $("cancel-reset-button").addEventListener("click", () => $("reset-dialog").close());
    $("confirm-reset-button").addEventListener("click", resetConfig);
    $("apply-json-button").addEventListener("click", applyRawConfig);
    $("raw-config-details").addEventListener("toggle", () => {
      if (!$("raw-config-details").open || state.rawEditorDirty || !state.config) return;
      $("config-editor").value = JSON.stringify(collectConfigFromUi(), null, 2);
    });
    $("config-editor").addEventListener("input", () => { state.rawEditorDirty = true; });
    $("clear-log-button").addEventListener("click", () => { state.log = []; $("event-log").textContent = ""; });
    $("copy-diagnostics-button").addEventListener("click", copyDiagnostics);
    $("device-info-button").addEventListener("click", () => openDialog("device-dialog"));
    for (const button of document.querySelectorAll("[data-dialog]")) button.addEventListener("click", () => openDialog(button.dataset.dialog));
  }

  bindEvents();
  Promise.all([loadConfig(), loadProfiles(), loadRecordings(), request("/api/status").then(renderSnapshot)])
    .catch((error) => setFeedback(error.message, true));
  connectEvents();
})();
