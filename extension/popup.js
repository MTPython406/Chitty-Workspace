// Check connection status to Chitty Workspace
async function checkStatus() {
  const dot = document.getElementById('status-dot');
  const text = document.getElementById('status-text');

  try {
    const resp = await fetch('http://127.0.0.1:8770/health');
    if (resp.ok) {
      dot.className = 'dot connected';
      text.textContent = 'Connected to Chitty Workspace';
    } else {
      dot.className = 'dot disconnected';
      text.textContent = 'Chitty Workspace not responding';
    }
  } catch (e) {
    dot.className = 'dot disconnected';
    text.textContent = 'Chitty Workspace not running';
  }
}

checkStatus();
