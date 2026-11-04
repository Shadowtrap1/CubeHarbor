// Server page: live logs over SSE and the RCON console.
// Settings come from data attributes on #log: base URL, first log to show, max kept lines.
const log = document.getElementById('log'), stateEl = document.getElementById('logstate');
const base = log.dataset.base;
const maxLines = Number(log.dataset.maxLines) || 4000;
let es;

function stream(c) {
  if (es) es.close();
  log.textContent = '';
  es = new EventSource(base + '/logs?container=' + c);
  stateEl.textContent = 'connecting…';
  es.onopen = () => stateEl.textContent = 'live';
  es.onerror = () => stateEl.textContent = 'disconnected (pod not running?)';
  const add = t => {
    const stick = log.scrollTop + log.clientHeight >= log.scrollHeight - 30;
    log.append(t + '\n');
    if (log.childNodes.length > maxLines) log.removeChild(log.firstChild);
    if (stick) log.scrollTop = log.scrollHeight;
  };
  es.onmessage = e => add(e.data);
  es.addEventListener('error', e => { if (e.data) add(e.data); });
}

document.querySelectorAll('[data-c]').forEach(b => b.onclick = () => {
  document.querySelectorAll('[data-c]').forEach(x => x.classList.toggle('on', x === b));
  stream(b.dataset.c);
});
stream(log.dataset.initial);

document.getElementById('console').onsubmit = async e => {
  e.preventDefault();
  const c = document.getElementById('cmd');
  const cmd = c.value.trim();
  if (!cmd) return;
  c.value = '';
  log.append('> ' + cmd + '\n');
  const r = await fetch(base + '/console', {
    method: 'POST',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    body: 'command=' + encodeURIComponent(cmd),
  });
  log.append((await r.text()) + '\n');
  log.scrollTop = log.scrollHeight;
};
