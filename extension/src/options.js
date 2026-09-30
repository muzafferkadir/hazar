/** Hazar Integration — options page. */
"use strict";

const DEFAULTS = {
  capture_enabled: true,
  capture_manifests: true,
  group_hls: true,
  min_size_bytes: 512 * 1024,
  excluded_hosts: [],
  deny_patterns: [],
  ytdlp_proxy: "",
  ytdlp_source_address: "",
  ytdlp_impersonate_hosts: [],
  ports: [8722, 8723, 8724, 8725, 8726, 8727, 8728, 8729, 8730],
};

const $ = (id) => document.getElementById(id);

function load() {
  chrome.storage.local.get(DEFAULTS, (settings) => {
    $("capture_enabled").checked = settings.capture_enabled !== false;
    $("capture_manifests").checked = settings.capture_manifests !== false;
    $("group_hls").checked = settings.group_hls !== false;
    $("min_size_mb").value = (Number(settings.min_size_bytes || 0) / (1024 * 1024)).toString();
    $("ports").value = (settings.ports || DEFAULTS.ports).join(", ");
    $("excluded_hosts").value = (settings.excluded_hosts || []).join("\n");
    $("ytdlp_proxy").value = settings.ytdlp_proxy || "";
    $("ytdlp_source_address").value = settings.ytdlp_source_address || "";
    $("ytdlp_impersonate_hosts").value = (settings.ytdlp_impersonate_hosts || []).join("\n");
    $("deny_patterns").value = (settings.deny_patterns || []).join("\n");
  });
}

function lines(value) {
  return String(value || "")
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
}

function save() {
  const ports = String($("ports").value || "")
    .split(",")
    .map((port) => Number(port.trim()))
    .filter((port) => Number.isInteger(port) && port > 0 && port < 65536);

  const patch = {
    capture_enabled: $("capture_enabled").checked,
    capture_manifests: $("capture_manifests").checked,
    group_hls: $("group_hls").checked,
    min_size_bytes: Math.max(0, Math.round(Number($("min_size_mb").value || 0) * 1024 * 1024)),
    ports: ports.length ? ports : DEFAULTS.ports,
    excluded_hosts: lines($("excluded_hosts").value),
    deny_patterns: lines($("deny_patterns").value),
    ytdlp_proxy: $("ytdlp_proxy").value.trim(),
    ytdlp_source_address: $("ytdlp_source_address").value.trim(),
    ytdlp_impersonate_hosts: lines($("ytdlp_impersonate_hosts").value),
  };

  if (patch.ytdlp_proxy && patch.ytdlp_proxy !== "direct") {
    try { const url = new URL(patch.ytdlp_proxy); if (url.username || url.password || !["http:", "https:", "socks4:", "socks5:", "socks5h:"].includes(url.protocol)) throw new Error(); }
    catch (_) { $("saved").textContent = "Proxy: şifresiz HTTP/SOCKS URL gir"; $("saved").style.opacity = "1"; return; }
  }
  $("saved").textContent = "kaydedildi";
  chrome.runtime.sendMessage({ type: "settings", patch }, () => {
    const saved = $("saved");
    saved.style.opacity = "1";
    setTimeout(() => {
      saved.style.opacity = "0";
    }, 1400);
  });
}

$("save").addEventListener("click", save);
load();
