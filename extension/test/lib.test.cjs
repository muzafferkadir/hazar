/**
 * Tiny assertion runner for the extension helpers (no dependencies).
 *   node extension/test/lib.test.js
 */
const assert = require("assert");
// lib.js is written as a classic script (importScripts in the extension), so it
// publishes itself on globalThis. Requiring it is enough.
require("../src/lib.js");
const lib = globalThis.HazarLib;
assert.ok(lib && typeof lib.pickKind === "function", "lib.js did not publish HazarLib");

let passed = 0;
function test(name, fn) {
  try {
    fn();
    passed += 1;
    console.log(`ok   ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}\n     ${error.message}`);
    process.exitCode = 1;
  }
}

test("pickKind detects hls/dash/file", () => {
  assert.equal(lib.pickKind("https://x/a/index.m3u8", null), "hls");
  assert.equal(lib.pickKind("https://x/a", "application/vnd.apple.mpegurl"), "hls");
  assert.equal(lib.pickKind("https://x/a/manifest.mpd", null), "dash");
  assert.equal(lib.pickKind("https://x/a", "application/dash+xml; charset=utf-8"), "dash");
  assert.equal(lib.pickKind("https://x/a/file.zip", null), "file");
});

test("shouldCapture honours scheme, size, holes and host exclusions", () => {
  const settings = {
    capture_enabled: true,
    min_size_bytes: 1024 * 1024,
    excluded_hosts: ["*.ads.example", "skip.example"],
  };
  assert.equal(lib.shouldCapture({ url: "blob:https://x/y", settings }).ok, false);
  assert.equal(
    lib.shouldCapture({ url: "https://cdn.example/a.zip", size: 2048, settings }).ok,
    false,
    "below min size",
  );
  assert.equal(lib.shouldCapture({ url: "https://cdn.example/a.zip", size: null, settings }).ok, true);
  assert.equal(lib.shouldCapture({ url: "https://a.ads.example/x.zip", settings }).ok, false);
  assert.equal(lib.shouldCapture({ url: "https://skip.example/x.zip", settings }).ok, false);
  assert.equal(lib.shouldCapture({ url: "https://cdn.example/x.zip", settings }).ok, true);
  assert.equal(
    lib.shouldCapture({ url: "https://cdn.example/x.zip", settings: { capture_enabled: false } }).ok,
    false,
  );
});

test("outputName prefers filename, then disposition, then url", () => {
  assert.equal(lib.outputName({ url: "https://x/a/b.zip" }), "b.zip");
  assert.equal(
    lib.outputName({ url: "https://x/stream", disposition: 'attachment; filename="movie.mp4"' }),
    "movie.mp4",
  );
  assert.equal(lib.outputName({ url: "https://x/v/index.m3u8" }), "index.ts");
  assert.equal(lib.outputName({ url: "https://x/v/index.m3u8", pageTitle: "My Film" }), "index.ts");
  assert.equal(lib.outputName({ url: "https://x/api/download", filename: "report.pdf" }), "report.pdf");
});

test("sanitizeFilename strips path and control characters", () => {
  assert.equal(lib.sanitizeFilename('a/b\\c:d*e?f"g<h>i|j'), "a_b_c_d_e_f_g_h_i_j");
  assert.equal(lib.sanitizeFilename("  ..hidden..  "), "hidden");
  assert.equal(lib.sanitizeFilename(""), "download");
});

test("cookieHeader joins cookies", () => {
  assert.equal(
    lib.cookieHeader([{ name: "a", value: "1" }, { name: "b", value: "2" }, { value: "x" }]),
    "a=1; b=2",
  );
  assert.equal(lib.cookieHeader([]), "");
});

test("groupSegments buckets by host+directory and sorts numerically", () => {
  const groups = lib.groupSegments([
    "https://cdn.example/hls/seg10.ts",
    "https://cdn.example/hls/seg2.ts",
    "https://cdn.example/hls/seg1.ts",
    "https://cdn.example/other/seg1.m4s",
    "https://cdn.example/page.html",
    "https://cdn.example/hls/seg2.ts",
  ]);
  assert.equal(groups.length, 2, "two buckets, html ignored");
  const [first] = groups;
  assert.deepEqual(first.segments, [
    "https://cdn.example/hls/seg1.ts",
    "https://cdn.example/hls/seg2.ts",
    "https://cdn.example/hls/seg10.ts",
  ]);
});

test("bestStream needs a minimum number of segments", () => {
  assert.equal(lib.bestStream(["https://x/a/1.ts", "https://x/a/2.ts"]), null);
  const stream = lib.bestStream(["https://x/a/1.ts", "https://x/a/2.ts", "https://x/a/3.ts"]);
  assert.equal(stream.segments.length, 3);
});

test("forwardableHeaders drops hop-by-hop and engine-managed headers", () => {
  const kept = lib.forwardableHeaders([
    { name: "Host", value: "x" },
    { name: "Range", value: "bytes=0-1" },
    { name: "Accept-Encoding", value: "gzip" },
    { name: "Authorization", value: "Bearer t" },
    { name: "X-Custom", value: "1" },
  ]);
  assert.deepEqual(kept, [
    ["Authorization", "Bearer t"],
    ["X-Custom", "1"],
  ]);
});

test("hostMatches supports wildcards and bare domains", () => {
  assert.equal(lib.hostMatches("a.ads.example", "*.ads.example"), true);
  assert.equal(lib.hostMatches("ads.example", "*.ads.example"), true);
  assert.equal(lib.hostMatches("example", "example"), true);
  assert.equal(lib.hostMatches("sub.example.com", "example.com"), true);
  assert.equal(lib.hostMatches("notexample.com", "example.com"), false);
});

test("segmentsFromPlaylist resolves absolute and relative URLs", () => {
  const playlist = [
    "#EXTM3U",
    "#EXT-X-TARGETDURATION:6",
    "#EXTINF:6,",
    "seg1.ts",
    "#EXTINF:6,",
    "/vod/seg2.ts?t=1",
    "#EXTINF:6,",
    "https://cdn.example/seg3.ts",
    "#EXT-X-ENDLIST",
    "",
  ].join(String.fromCharCode(10));
  assert.deepEqual(
    lib.segmentsFromPlaylist(playlist, "https://cdn.example/vod/index.m3u8"),
    ["https://cdn.example/vod/seg1.ts", "https://cdn.example/vod/seg2.ts?t=1", "https://cdn.example/seg3.ts"],
  );
  assert.deepEqual(lib.segmentsFromPlaylist("<html>nope</html>", "https://x/"), []);
});

console.log(`\n${passed} test(s) passed`);
