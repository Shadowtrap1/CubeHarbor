// Server list: refresh the table rows every `data-refresh` seconds (0 disables).
const rows = document.getElementById('rows');
const every = Number(rows.dataset.refresh) * 1000;
if (every > 0) {
  setInterval(async () => {
    try {
      const r = await fetch('/fragments/servers');
      if (r.ok) rows.innerHTML = await r.text();
    } catch (e) {}
  }, every);
}
