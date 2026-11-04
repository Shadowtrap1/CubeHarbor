// "New server" page: tabs, compose file loading and modpack search.
const tabs = document.querySelectorAll('.tabs button');
const show = t => {
  tabs.forEach(x => x.classList.toggle('on', x.dataset.tab === t));
  ['quick', 'compose'].forEach(n => document.getElementById('tab-' + n).style.display = n === t ? '' : 'none');
};
tabs.forEach(b => b.onclick = () => show(b.dataset.tab));
if (location.hash === '#quick') show('quick');

const src = document.getElementById('source'), box = document.getElementById('pack-box');
const togglePack = () => box.style.display = src.value === 'modpack' ? '' : 'none';
src.onchange = togglePack;
togglePack();

// Several compose files -> one multi-document YAML stream. A file inside a folder gets the folder
// name as its compose project name (used when nothing better names the server).
async function load(files, onlyCompose) {
  const fs = [...files].filter(f => !onlyCompose || /(^|\/)(docker-)?compose(\.[\w-]+)?\.ya?ml$/.test(f.webkitRelativePath || f.name));
  if (!fs.length) { document.getElementById('filestate').textContent = 'no compose files found'; return; }
  const docs = await Promise.all(fs.map(async f => {
    let t = await f.text();
    const path = f.webkitRelativePath || f.name;
    const dir = path.split('/').slice(-2, -1)[0];
    if (dir && !/^name\s*:/m.test(t) && /^services\s*:/m.test(t)) t = 'name: ' + dir + '\n' + t;
    return '# from ' + path + '\n' + t.trim() + '\n';
  }));
  document.querySelector('textarea[name=compose]').value = docs.join('---\n');
  document.getElementById('filestate').textContent = fs.length + ' file(s) loaded';
}
document.getElementById('fromcurrent').onclick = async () => {
  const r = await fetch('/compose/export');
  document.querySelector('textarea[name=compose]').value = await r.text();
  document.getElementById('filestate').textContent = 'exported ' + (r.ok ? 'current servers' : 'failed');
};
document.getElementById('files').onchange = e => load(e.target.files, false);
document.getElementById('folder').onchange = e => load(e.target.files, true);

const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
async function search() {
  const q = document.getElementById('q').value.trim();
  if (!q) return;
  const r = document.getElementById('results');
  r.textContent = 'Searching…';
  try {
    const res = await fetch('/api/modpacks/search?source=' + document.getElementById('src').value + '&q=' + encodeURIComponent(q));
    const hits = await res.json();
    r.innerHTML = hits.map(h => `<div class="pack" data-id="${esc(h.id)}"><img src="${esc(h.icon || '')}" alt=""><div><b>${esc(h.title)}</b><small>${esc(h.description)}</small><small>${Number(h.downloads).toLocaleString()} downloads</small></div></div>`).join('') || 'No results';
    r.querySelectorAll('.pack').forEach(p => p.onclick = () => {
      document.getElementById('modpack').value = p.dataset.id;
      r.querySelectorAll('.pack').forEach(x => x.style.borderColor = '');
      p.style.borderColor = 'var(--accent)';
    });
  } catch (e) {
    r.textContent = 'Search failed: ' + e;
  }
}
document.getElementById('search').onclick = search;
document.getElementById('q').onkeydown = e => { if (e.key === 'Enter') { e.preventDefault(); search(); } };
