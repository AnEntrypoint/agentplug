const port = process.argv[2];
const extPath = process.argv[3];

async function main() {
  const verRes = await fetch(`http://127.0.0.1:${port}/json/version`);
  const ver = await verRes.json();
  const wsUrl = ver.webSocketDebuggerUrl;
  if (!wsUrl) {
    console.log(JSON.stringify({ error: 'no webSocketDebuggerUrl from /json/version' }));
    return;
  }
  const ws = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', () => resolve());
    ws.addEventListener('error', (e) => reject(new Error(String(e && e.message || 'ws error'))));
  });
  const result = await new Promise((resolve, reject) => {
    const id = 1;
    const timer = setTimeout(() => reject(new Error('timeout waiting for Extensions.loadUnpacked response')), 8000);
    ws.addEventListener('message', (ev) => {
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch (_) {
        return;
      }
      if (msg.id === id) {
        clearTimeout(timer);
        resolve(msg);
      }
    });
    ws.send(JSON.stringify({ id, method: 'Extensions.loadUnpacked', params: { path: extPath } }));
  });
  ws.close();
  if (result.error) {
    console.log(JSON.stringify({ error: result.error.message || JSON.stringify(result.error) }));
    return;
  }
  console.log(JSON.stringify({ id: result.result && result.result.id }));
}

main().catch((e) => {
  console.log(JSON.stringify({ error: String((e && e.message) || e) }));
});
