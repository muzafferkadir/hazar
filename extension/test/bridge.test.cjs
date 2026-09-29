/**
 * End-to-end protocol test: the extension's helpers build a grab exactly the
 * way background.js does, a real Hazar app process receives it over the
 * loopback WebSocket, and we assert both directions.
 *
 *   node extension/test/bridge.test.cjs
 *
 * Requires: cargo (builds crates/hazar-localapi/examples/echo_server.rs).
 */
const assert = require("assert");
const path = require("path");
const { spawn } = require("child_process");

require("../src/lib.js");
const Lib = globalThis.HazarLib;
const ROOT = path.join(__dirname, "..", "..");

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function startApp() {
  const child = spawn("cargo", ["run", "-q", "--example", "echo_server"], {
    cwd: ROOT,
    stdio: ["ignore", "pipe", "pipe"],
  });

  const lines = [];
  let port = null;
  let readyResolve;
  let grabResolve;
  const ready = new Promise((resolve) => (readyResolve = resolve));
  const grab = new Promise((resolve) => (grabResolve = resolve));

  let buffer = "";
  child.stdout.on("data", (chunk) => {
    buffer += chunk.toString();
    let index;
    while ((index = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, index).trim();
      buffer = buffer.slice(index + 1);
      if (!line) continue;
      lines.push(line);
      if (line.startsWith("PORT ")) port = Number(line.slice(5));
      if (line === "READY") readyResolve();
      if (line.startsWith("GRAB ")) {
        const firstSpace = line.indexOf(" ", 5);
        grabResolve({
          id: line.slice(5, firstSpace),
          request: JSON.parse(line.slice(firstSpace + 1)),
          raw: line,
        });
      }
    }
  });
  child.stderr.on("data", (chunk) => process.stderr.write(chunk));

  return { child, ready, grab, lines, port: () => port };
}

function connect(port, subprotocol) {
  const url = `ws://127.0.0.1:${port}/hazar`;
  const socket = subprotocol ? new WebSocket(url, subprotocol) : new WebSocket(url);
  const inbox = [];
  let waiter = null;

  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (waiter) {
      const resolve = waiter;
      waiter = null;
      resolve(message);
    } else {
      inbox.push(message);
    }
  });

  const next = (timeoutMs = 5000) =>
    new Promise((resolve, reject) => {
      if (inbox.length) return resolve(inbox.shift());
      const timer = setTimeout(() => {
        waiter = null;
        reject(new Error("timed out waiting for a message from the app"));
      }, timeoutMs);
      waiter = (message) => {
        clearTimeout(timer);
        resolve(message);
      };
    });

  const open = new Promise((resolve, reject) => {
    socket.addEventListener("open", () => resolve());
    socket.addEventListener("error", () => reject(new Error("websocket error")));
    socket.addEventListener("close", () => reject(new Error("websocket closed early")));
  });

  return { socket, next, open, inbox };
}

let failures = 0;
async function step(name, fn) {
  try {
    await fn();
    console.log(`ok   ${name}`);
  } catch (error) {
    failures += 1;
    console.error(`FAIL ${name}\n     ${error.message}`);
    process.exitCode = 1;
  }
}

(async () => {
  const app = startApp();
  const hardStop = setTimeout(() => {
    console.error("FAIL global timeout");
    app.child.kill("SIGKILL");
    process.exit(1);
  }, 180000);

  await app.ready;

  await step("handshake returns a session the extension can use", async () => {
    const client = connect(app.port(), "hazar.v1");
    await client.open;
    client.socket.send(
      JSON.stringify({
        type: "hello",
        protocol: 1,
        client: "chrome",
        extension_id: "test",
        version: "0.1.0",
      }),
    );
    const hello = await client.next();
    assert.equal(hello.type, "hello_ok", JSON.stringify(hello));
    assert.ok(hello.session, "no session token");
    assert.ok(hello.features.includes("hls"));
    assert.equal(hello.settings.connections, 8);
    client.socket.close();
  });

  await step("a grab built by the extension helpers is accepted", async () => {
    const url = "https://cdn.example.com/vod/high/index.m3u8?token=abc";
    const client = connect(app.port(), "hazar.v1");
    await client.open;
    client.socket.send(JSON.stringify({ type: "hello", protocol: 1, client: "chrome" }));
    const hello = await client.next();

    const request = {
      url,
      kind: Lib.pickKind(url, "application/vnd.apple.mpegurl"),
      filename: Lib.outputName({ url, mime: "application/vnd.apple.mpegurl", pageTitle: "Film" }),
      mime: "application/vnd.apple.mpegurl",
      size: null,
      method: "GET",
      referer: "https://site.example/watch",
      user_agent: "Mozilla/5.0 (test)",
      cookie: Lib.cookieHeader([{ name: "sid", value: "abc123" }]),
      headers: Lib.forwardableHeaders([{ name: "Authorization", value: "Bearer t" }]),
      page_url: "https://site.example/watch",
      tab_id: 7,
      save_dir: null,
      segments: ["https://cdn.example.com/vod/high/seg0.ts"],
      manifest: null,
    };

    client.socket.send(
      JSON.stringify({ type: "grab", session: hello.session, id: "grab-42", request }),
    );

    const received = await app.grab;
    assert.equal(received.id, "grab-42");
    assert.equal(received.request.kind, "hls");
    assert.equal(received.request.url, url);
    assert.equal(received.request.cookie, "sid=abc123");
    assert.deepEqual(received.request.headers, [["Authorization", "Bearer t"]]);
    assert.deepEqual(received.request.segments, request.segments);
    assert.equal(received.request.tab_id, 7);
    assert.equal(received.request.filename, request.filename);

    const ack = await client.next();
    assert.equal(ack.type, "grab_ack");
    assert.equal(ack.id, "grab-42");
    assert.equal(ack.state, "queued");

    client.socket.send(JSON.stringify({ type: "cancel", session: hello.session, id: "grab-42" }));
    const cancelLine = await new Promise((resolve) => {
      const timer = setInterval(() => {
        const found = app.lines.find((line) => line === "CANCEL grab-42");
        if (found) {
          clearInterval(timer);
          resolve(found);
        }
      }, 50);
      setTimeout(() => {
        clearInterval(timer);
        resolve(null);
      }, 5000);
    });
    assert.ok(cancelLine, "cancel was not delivered");

    client.socket.send(JSON.stringify({ type: "ping", session: hello.session, t: 1234 }));
    const pong = await client.next();
    assert.equal(pong.type, "pong");
    assert.equal(pong.t, 1234);

    client.socket.close();
  });

  await step("connections without the hazar.v1 subprotocol are refused", async () => {
    const client = connect(app.port(), null);
    let refused = false;
    try {
      await client.open;
    } catch (_) {
      refused = true;
    }
    assert.ok(refused, "connection without subprotocol was accepted");
  });

  await wait(200);
  app.child.kill("SIGTERM");
  clearTimeout(hardStop);

  if (!failures) console.log("\nall bridge tests passed");
})();
