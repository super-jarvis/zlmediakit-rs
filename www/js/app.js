/* ── state ────────────────────────────────────────── */
let state = {
  streams: [],
  proxies: [],
  recordings: {},
  forwarders: [],
  ffmpegSources: [],
  serverInfo: null,
  statistics: null,
  currentPlayer: null,
  refreshTimers: [],
};

/* ── router ───────────────────────────────────────── */
function navigate(tab) {
  document.querySelectorAll('.page').forEach(p => p.classList.remove('active'));
  document.querySelectorAll('.sidebar-nav a').forEach(a => a.classList.remove('active'));
  const page = document.getElementById('page-' + tab);
  if (page) page.classList.add('active');
  const link = document.querySelector(`.sidebar-nav a[data-tab="${tab}"]`);
  if (link) link.classList.add('active');
  window.history.replaceState(null, '', '#' + tab);
  const initFn = tabInitializers[tab];
  if (initFn) initFn();
}

const tabInitializers = {};

/* ── API helpers ──────────────────────────────────── */
const API_BASE = '/index/api';

async function apiGet(endpoint, params = {}) {
  const qs = Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null && v !== '')
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
    .join('&');
  const url = `${API_BASE}/${endpoint}${qs ? '?' + qs : ''}`;
  const res = await fetch(url);
  if (!res.ok) throw new Error(`HTTP ${res.status}: ${res.statusText}`);
  return res.json();
}

function showAlert(containerId, type, msg, timeout = 4000) {
  const container = document.getElementById(containerId);
  if (!container) return;
  const div = document.createElement('div');
  div.className = `alert alert-${type}`;
  div.textContent = msg;
  container.prepend(div);
  if (timeout > 0) setTimeout(() => div.remove(), timeout);
}

function clearAlerts(containerId) {
  const container = document.getElementById(containerId);
  if (container) container.innerHTML = '';
}

/* ── Dashboard ────────────────────────────────────── */
async function loadDashboard() {
  try {
    const data = await apiGet('getMediaList');
    state.streams = data.result || [];
    renderStreamTable();
    renderServerStats();
  } catch (e) {
    document.getElementById('stream-table-body').innerHTML =
      `<tr class="empty-row"><td colspan="8">加载失败: ${e.message}</td></tr>`;
  }
}

function renderServerStats() {
  const list = state.streams;
  document.getElementById('stat-streams').textContent = list.length;
  let viewers = 0;
  list.forEach(s => { viewers += s.readerCount || 0; });
  document.getElementById('stat-viewers').textContent = viewers;
  const aliveStreams = list.filter(s => s.createTime).length;
  document.getElementById('stat-alive').textContent = aliveStreams;
}

function renderStreamTable() {
  const tb = document.getElementById('stream-table-body');
  const list = state.streams;
  if (!list.length) {
    tb.innerHTML = '<tr class="empty-row"><td colspan="8">暂无活跃流</td></tr>';
    return;
  }
  tb.innerHTML = list.map(s => {
    const app = s.app || 'live';
    const stream = s.stream || '';
    const vtracks = (s.tracks || []).filter(t => t.codec_type === 'video');
    const atracks = (s.tracks || []).filter(t => t.codec_type === 'audio');
    const vinfo = vtracks.map(t =>
      `${t.codec_id} ${t.width || '?'}x${t.height || '?'} @${t.fps || '?'}fps`
    ).join(', ') || '-';
    const ainfo = atracks.map(t =>
      `${t.codec_id} ${((t.sample_rate || 0) / 1000).toFixed(1)}kHz ${t.channels || 0}ch`
    ).join(', ') || '-';
    const alive = s.createTime
      ? fmtDuration((Date.now() - s.createTime) / 1000)
      : '-';
    return `<tr>
      <td>${escHtml(app)}</td>
      <td><a href="#" onclick="setPlayerUrl('${escHtml(app)}','${escHtml(stream)}');return false;">${escHtml(stream)}</a></td>
      <td><span class="badge badge-green">直播中</span></td>
      <td>${s.readerCount || 0}</td>
      <td style="font-size:12px">${alive}</td>
      <td style="font-size:11px;max-width:280px;overflow:hidden;text-overflow:ellipsis;">${vinfo}</td>
      <td style="font-size:11px;max-width:200px;overflow:hidden;text-overflow:ellipsis;">${ainfo}</td>
      <td>
        <button class="btn btn-danger btn-sm" onclick="closeStream('${escHtml(app)}','${escHtml(stream)}')">✕ 关闭</button>
      </td>
    </tr>`;
  }).join('');
}

function fmtDuration(sec) {
  if (sec < 60) return Math.floor(sec) + '秒';
  if (sec < 3600) return Math.floor(sec / 60) + '分 ' + Math.floor(sec % 60) + '秒';
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  return h + '时 ' + m + '分';
}

function escHtml(s) {
  if (!s) return '';
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

async function closeStream(app, stream) {
  if (!confirm(`确定关闭流: ${app}/${stream} ?`)) return;
  try {
    await apiGet('closeStream', { app, stream, vhost: '__defaultVhost__' });
    showAlert('dash-alerts', 'success', `已关闭 ${app}/${stream}`);
    loadDashboard();
  } catch (e) {
    showAlert('dash-alerts', 'error', `操作失败: ${e.message}`);
  }
}

function setPlayerUrl(app, stream) {
  const sel = document.getElementById('dash-player-type');
  const port = window.location.port ? ':' + window.location.port : '';
  const host = window.location.hostname + port;
  const scheme = window.location.protocol === 'https:' ? 'https' : 'http';
  switch (sel.value) {
    case 'hls':
      document.getElementById('dash-url').value = `/${app}/${stream}/hls.m3u8`;
      break;
    case 'wsflv':
      document.getElementById('dash-url').value = `ws://${host}/${app}/${stream}.flv`;
      break;
    default:
      document.getElementById('dash-url').value = `/${app}/${stream}.flv`;
  }
  playDashStream();
}

function playDashStream() {
  stopPlayer();
  const url = document.getElementById('dash-url').value.trim();
  if (!url) return;
  const playerType = document.getElementById('dash-player-type').value;
  startPlayer('dash-video', 'dash-player-info', 'dash-player-error', 'dash-player-container', url, playerType);
}

tabInitializers.dashboard = () => {
  loadDashboard();
  if (!state.refreshTimers.includes('dash')) {
    const id = setInterval(loadDashboard, 3000);
    state.refreshTimers.push('dash');
    window._dashTimer = id;
  }
};

/* ── Player Core ──────────────────────────────────── */
function startPlayer(videoId, infoId, errorId, containerId, url, playerType) {
  stopPlayer();
  const video = document.getElementById(videoId);
  const info = document.getElementById(infoId);
  const errEl = document.getElementById(errorId);
  const container = document.getElementById(containerId);
  if (!video || !info || !errEl || !container) return;
  errEl.style.display = 'none';
  container.style.display = 'block';

  const isAbs = url.startsWith('http://') || url.startsWith('https://') || url.startsWith('ws://');
  const host = window.location.host;
  const proto = window.location.protocol === 'https:' ? 'https' : 'http';
  const fullUrl = isAbs ? url : proto + '://' + host + url;

  if (url.endsWith('.m3u8') || playerType === 'hls') {
    info.textContent = 'HLS: ' + fullUrl;
    if (typeof Hls !== 'undefined' && Hls.isSupported()) {
      const hls = new Hls();
      hls.loadSource(fullUrl);
      hls.attachMedia(video);
      hls.on(Hls.Events.MANIFEST_PARSED, () => video.play().catch(() => {}));
      hls.on(Hls.Events.ERROR, (e, d) => {
        if (d.fatal) { errEl.textContent = 'HLS 错误: ' + d.type + ' ' + d.details; errEl.style.display = 'block'; }
      });
      state.currentPlayer = hls;
    } else if (video.canPlayType('application/vnd.apple.mpegurl')) {
      video.src = fullUrl;
      state.currentPlayer = video;
    } else {
      errEl.textContent = '当前浏览器不支持 HLS 播放';
      errEl.style.display = 'block';
    }
  } else if (url.startsWith('ws://') || url.endsWith('.flv') || playerType === 'flv' || playerType === 'wsflv') {
    const label = url.startsWith('ws://') ? 'WebSocket-FLV' : 'HTTP-FLV';
    info.textContent = label + ': ' + (url.startsWith('ws://') ? url : fullUrl);
    if (typeof flvjs !== 'undefined' && flvjs.isSupported()) {
      const f = flvjs.createPlayer(
        { type: 'flv', url: url.startsWith('ws://') ? url : fullUrl, isLive: true },
        { enableWorker: false }
      );
      f.attachMediaElement(video);
      f.load();
      f.play();
      state.currentPlayer = f;
    } else {
      errEl.textContent = '当前浏览器不支持 FLV 播放';
      errEl.style.display = 'block';
    }
  } else {
    info.textContent = url;
    video.src = fullUrl;
    state.currentPlayer = video;
  }
}

function stopPlayer() {
  const video = document.getElementById('dash-video');
  if (state.currentPlayer) {
    if (state.currentPlayer.destroy) state.currentPlayer.destroy();
    else if (state.currentPlayer.pause) { state.currentPlayer.pause(); video.src = ''; }
    state.currentPlayer = null;
  }
  const c = document.getElementById('dash-player-container');
  if (c) c.style.display = 'none';
}

/* ── Proxies ──────────────────────────────────────── */
async function loadProxies() {
  try {
    const data = await apiGet('getStreamProxyList');
    state.proxies = data.result || [];
    renderProxyTable();
  } catch (e) {
    document.getElementById('proxy-table-body').innerHTML =
      `<tr class="empty-row"><td colspan="6">加载失败: ${e.message}</td></tr>`;
  }
}

function renderProxyTable() {
  const tb = document.getElementById('proxy-table-body');
  if (!state.proxies.length) {
    tb.innerHTML = '<tr class="empty-row"><td colspan="6">暂无代理</td></tr>';
    return;
  }
  tb.innerHTML = state.proxies.map(p => {
    const app = p.app || 'live';
    const stream = p.stream || '';
    return `<tr>
      <td>${escHtml(app)}</td>
      <td>${escHtml(stream)}</td>
      <td style="max-width:300px;overflow:hidden;text-overflow:ellipsis;">${escHtml(p.url || p.src_url || '-')}</td>
      <td><span class="badge badge-green">运行中</span></td>
      <td>${p.readerCount || 0}</td>
      <td><button class="btn btn-danger btn-sm" onclick="deleteProxy('${escHtml(app)}','${escHtml(stream)}')">✕ 删除</button></td>
    </tr>`;
  }).join('');
}

async function addProxy() {
  const form = document.getElementById('proxy-form');
  const url = form.querySelector('[name=url]').value.trim();
  const app = form.querySelector('[name=app]').value.trim() || 'live';
  const stream = form.querySelector('[name=stream]').value.trim();
  if (!url) { showAlert('proxy-alerts', 'error', '请填写流地址'); return; }
  if (!stream) { showAlert('proxy-alerts', 'error', '请填写流名称'); return; }
  const btn = form.querySelector('.btn-primary');
  btn.disabled = true;
  try {
    await apiGet('addStreamProxy', { url, app, stream, vhost: '__defaultVhost__' });
    showAlert('proxy-alerts', 'success', `代理添加成功: ${url}`);
    form.reset();
    loadProxies();
  } catch (e) {
    showAlert('proxy-alerts', 'error', `添加失败: ${e.message}`);
  } finally {
    btn.disabled = false;
  }
}

async function deleteProxy(app, stream) {
  if (!confirm(`确定删除代理 ${app}/${stream} ?`)) return;
  try {
    await apiGet('delStreamProxy', { app, stream, vhost: '__defaultVhost__' });
    showAlert('proxy-alerts', 'success', `已删除 ${app}/${stream}`);
    loadProxies();
  } catch (e) {
    showAlert('proxy-alerts', 'error', `删除失败: ${e.message}`);
  }
}

tabInitializers.proxies = () => { loadProxies(); };

/* ── Recording ────────────────────────────────────── */
async function loadRecordingStatus() {
  try {
    const streams = await apiGet('getMediaList');
    const recStatus = {};
    for (const s of (streams.result || [])) {
      try {
        const r = await apiGet('isRecording', {
          vhost: '__defaultVhost__', app: s.app || 'live', stream: s.stream || ''
        });
        recStatus[(s.app || 'live') + '/' + (s.stream || '')] = r.result || {};
      } catch (_) {}
    }
    state.recordings = recStatus;
    renderRecordingTable(streams.result || []);
  } catch (e) {
    document.getElementById('rec-table-body').innerHTML =
      `<tr class="empty-row"><td colspan="6">加载失败: ${e.message}</td></tr>`;
  }
}

function renderRecordingTable(streams) {
  const tb = document.getElementById('rec-table-body');
  if (!streams.length) {
    tb.innerHTML = '<tr class="empty-row"><td colspan="6">暂无活跃流</td></tr>';
    return;
  }
  tb.innerHTML = streams.map(s => {
    const app = s.app || 'live';
    const stream = s.stream || '';
    const key = app + '/' + stream;
    const rec = state.recordings[key] || {};
    const hlsOn = rec.hls || false;
    const flvOn = rec.flv || false;
    const mp4On = rec.mp4 || false;
    return `<tr>
      <td>${escHtml(app)}</td>
      <td>${escHtml(stream)}</td>
      <td>
        ${hlsOn ? '<span class="badge badge-green">HLS</span>' : ''}
        ${flvOn ? '<span class="badge badge-blue">FLV</span>' : ''}
        ${mp4On ? '<span class="badge badge-orange">MP4</span>' : ''}
        ${!hlsOn && !flvOn && !mp4On ? '<span class="badge badge-gray">未录制</span>' : ''}
      </td>
      <td>
        ${!hlsOn ? `<button class="btn btn-sm" onclick="startRec('${escHtml(app)}','${escHtml(stream)}','hls')">HLS</button>` : ''}
        ${!flvOn ? `<button class="btn btn-sm" onclick="startRec('${escHtml(app)}','${escHtml(stream)}','flv')">FLV</button>` : ''}
        ${!mp4On ? `<button class="btn btn-sm" onclick="startRec('${escHtml(app)}','${escHtml(stream)}','mp4')">MP4</button>` : ''}
      </td>
      <td>
        <button class="btn btn-danger btn-sm" onclick="stopRec('${escHtml(app)}','${escHtml(stream)}')">停止全部</button>
      </td>
      <td style="font-size:12px;color:var(--text-secondary)">${rec.hls ? 'hls:'+ (rec.hls_path||'') : ''}</td>
    </tr>`;
  }).join('');
}

async function startRec(app, stream, type) {
  try {
    await apiGet('startRecord', { vhost: '__defaultVhost__', app, stream, type });
    showAlert('rec-alerts', 'success', `${type}录制已启动: ${app}/${stream}`);
    loadRecordingStatus();
  } catch (e) {
    showAlert('rec-alerts', 'error', `启动失败: ${e.message}`);
  }
}

async function stopRec(app, stream) {
  if (!confirm(`确定停止 ${app}/${stream} 的全部录制?`)) return;
  try {
    await apiGet('stopRecord', { vhost: '__defaultVhost__', app, stream });
    showAlert('rec-alerts', 'success', `录制已停止: ${app}/${stream}`);
    loadRecordingStatus();
  } catch (e) {
    showAlert('rec-alerts', 'error', `停止失败: ${e.message}`);
  }
}

tabInitializers.recording = () => { loadRecordingStatus(); };

/* ── Forwarding ───────────────────────────────────── */
async function loadForwarders() {
  try {
    const data = await apiGet('getStreamPusherList');
    state.forwarders = data.result || [];
    renderForwarderTable();
  } catch (e) {
    document.getElementById('forward-table-body').innerHTML =
      `<tr class="empty-row"><td colspan="5">加载失败: ${e.message}</td></tr>`;
  }
}

function renderForwarderTable() {
  const tb = document.getElementById('forward-table-body');
  if (!state.forwarders.length) {
    tb.innerHTML = '<tr class="empty-row"><td colspan="5">暂无转发</td></tr>';
    return;
  }
  tb.innerHTML = state.forwarders.map(f => {
    const app = f.app || 'live';
    const stream = f.stream || '';
    return `<tr>
      <td>${escHtml(app)}</td>
      <td>${escHtml(stream)}</td>
      <td style="max-width:300px;overflow:hidden;text-overflow:ellipsis;">${escHtml(f.dst_url || f.url || '-')}</td>
      <td><span class="badge badge-green">运行中</span></td>
      <td><button class="btn btn-danger btn-sm" onclick="deleteForwarder('${escHtml(app)}','${escHtml(stream)}')">✕ 停止</button></td>
    </tr>`;
  }).join('');
}

async function addForwarder() {
  const form = document.getElementById('forward-form');
  const dstUrl = form.querySelector('[name=dst_url]').value.trim();
  const app = form.querySelector('[name=app]').value.trim() || 'live';
  const stream = form.querySelector('[name=stream]').value.trim();
  if (!dstUrl) { showAlert('forward-alerts', 'error', '请填写目标地址'); return; }
  if (!stream) { showAlert('forward-alerts', 'error', '请填写流名称'); return; }
  const btn = form.querySelector('.btn-primary');
  btn.disabled = true;
  try {
    await apiGet('addStreamPusher', { dst_url: dstUrl, app, stream, vhost: '__defaultVhost__' });
    showAlert('forward-alerts', 'success', `转发添加成功: ${dstUrl}`);
    form.reset();
    loadForwarders();
  } catch (e) {
    showAlert('forward-alerts', 'error', `添加失败: ${e.message}`);
  } finally {
    btn.disabled = false;
  }
}

async function deleteForwarder(app, stream) {
  if (!confirm(`确定停止转发 ${app}/${stream} ?`)) return;
  try {
    await apiGet('delStreamPusher', { app, stream, vhost: '__defaultVhost__' });
    showAlert('forward-alerts', 'success', `已停止 ${app}/${stream}`);
    loadForwarders();
  } catch (e) {
    showAlert('forward-alerts', 'error', `停止失败: ${e.message}`);
  }
}

tabInitializers.forwarding = () => { loadForwarders(); };

/* ── FFmpeg Source ─────────────────────────────────── */
async function loadFFmpegSources() {
  try {
    const data = await apiGet('getFFmpegSourceList');
    state.ffmpegSources = data.result || [];
    renderFFmpegTable();
  } catch (e) {
    document.getElementById('ffmpeg-table-body').innerHTML =
      `<tr class="empty-row"><td colspan="6">加载失败: ${e.message}</td></tr>`;
  }
}

function renderFFmpegTable() {
  const tb = document.getElementById('ffmpeg-table-body');
  if (!state.ffmpegSources.length) {
    tb.innerHTML = '<tr class="empty-row"><td colspan="6">暂无 FFmpeg 源</td></tr>';
    return;
  }
  tb.innerHTML = state.ffmpegSources.map(f => {
    const app = f.app || 'live';
    const stream = f.stream || '';
    return `<tr>
      <td style="max-width:200px;overflow:hidden;text-overflow:ellipsis;">${escHtml(f.src_url || '-')}</td>
      <td style="max-width:200px;overflow:hidden;text-overflow:ellipsis;">${escHtml(f.dst_url || '-')}</td>
      <td>${escHtml(app)}</td>
      <td>${escHtml(stream)}</td>
      <td><span class="badge badge-green">运行中</span></td>
      <td><button class="btn btn-danger btn-sm" onclick="deleteFFmpeg('${escHtml(app)}','${escHtml(stream)}')">✕ 删除</button></td>
    </tr>`;
  }).join('');
}

async function addFFmpegSource() {
  const form = document.getElementById('ffmpeg-form');
  const srcUrl = form.querySelector('[name=src_url]').value.trim();
  const dstUrl = form.querySelector('[name=dst_url]').value.trim();
  const app = form.querySelector('[name=app]').value.trim() || 'live';
  const stream = form.querySelector('[name=stream]').value.trim();
  const timeoutMs = form.querySelector('[name=timeout_ms]').value.trim() || '10000';
  if (!srcUrl) { showAlert('ffmpeg-alerts', 'error', '请填写输入地址'); return; }
  if (!dstUrl) { showAlert('ffmpeg-alerts', 'error', '请填写输出地址'); return; }
  if (!stream) { showAlert('ffmpeg-alerts', 'error', '请填写流名称'); return; }
  const btn = form.querySelector('.btn-primary');
  btn.disabled = true;
  try {
    await apiGet('addFFmpegSource', {
      src_url: srcUrl, dst_url: dstUrl,
      app, stream, vhost: '__defaultVhost__',
      timeout_ms: parseInt(timeoutMs) || 10000
    });
    showAlert('ffmpeg-alerts', 'success', 'FFmpeg 源添加成功');
    form.reset();
    loadFFmpegSources();
  } catch (e) {
    showAlert('ffmpeg-alerts', 'error', `添加失败: ${e.message}`);
  } finally {
    btn.disabled = false;
  }
}

async function deleteFFmpeg(app, stream) {
  if (!confirm(`确定删除 FFmpeg 源 ${app}/${stream} ?`)) return;
  try {
    await apiGet('delFFmpegSource', { app, stream, vhost: '__defaultVhost__' });
    showAlert('ffmpeg-alerts', 'success', `已删除 ${app}/${stream}`);
    loadFFmpegSources();
  } catch (e) {
    showAlert('ffmpeg-alerts', 'error', `删除失败: ${e.message}`);
  }
}

tabInitializers.ffmpeg = () => { loadFFmpegSources(); };

/* ── Server ────────────────────────────────────────── */
async function loadServerInfo() {
  try {
    const [cfg, stat] = await Promise.all([
      apiGet('getServerConfig'),
      apiGet('getStatistic'),
    ]);
    state.serverInfo = cfg.result || {};
    state.statistics = stat.result || [];
    renderServerPage();
  } catch (e) {
    document.getElementById('server-info').innerHTML =
      `<div class="alert alert-error">加载失败: ${e.message}</div>`;
  }
}

function renderServerPage() {
  const info = state.serverInfo;
  const stats = state.statistics;
  let html = '<div class="card"><div class="card-header"><h3>服务器配置</h3></div>';
  if (info && Object.keys(info).length) {
    html += '<div class="code-block">' + escHtml(JSON.stringify(info, null, 2)) + '</div>';
  } else {
    html += '<p class="text-secondary text-small">暂无配置数据</p>';
  }
  html += '</div>';

  html += '<div class="card"><div class="card-header"><h3>流统计信息</h3></div>';
  if (stats && stats.length) {
    html += '<table><thead><tr><th>应用</th><th>流名称</th><th>存活时间</th><th>视频轨</th><th>音频轨</th></tr></thead><tbody>';
    stats.forEach(s => {
      const alive = s.alive_seconds
        ? fmtDuration(s.alive_seconds)
        : (s.createTime ? fmtDuration((Date.now() - s.createTime) / 1000) : '-');
      html += `<tr>
        <td>${escHtml(s.app || '-')}</td>
        <td>${escHtml(s.stream || '-')}</td>
        <td>${alive}</td>
        <td>${s.video_tracks || 0}</td>
        <td>${s.audio_tracks || 0}</td>
      </tr>`;
    });
    html += '</tbody></table>';
  } else {
    html += '<p class="text-secondary text-small">暂无统计数据</p>';
  }
  html += '</div>';

  html += '<div class="card"><div class="card-header"><h3>流地址参考</h3></div>';
  const port = window.location.port ? ':' + window.location.port : '';
  const host = window.location.hostname + port;
  html += `<table>
    <tr><th>协议</th><th>地址</th></tr>
    <tr><td>RTMP</td><td><code>rtmp://${host}/live/stream</code></td></tr>
    <tr><td>RTSP</td><td><code>rtsp://${host}/live/stream</code></td></tr>
    <tr><td>HTTP-FLV</td><td><code>http://${host}/live/stream.flv</code></td></tr>
    <tr><td>HLS</td><td><code>http://${host}/live/stream/hls.m3u8</code></td></tr>
    <tr><td>WebSocket-FLV</td><td><code>ws://${host}/live/stream.flv</code></td></tr>
    <tr><td>WebRTC 播放</td><td><code>http://${host}/webrtc/play/app/stream</code></td></tr>
    <tr><td>WebRTC 推流</td><td><code>http://${host}/webrtc/publish/app/stream</code></td></tr>
    <tr><td>API 接口</td><td><code>http://${host}/index/api/getMediaList</code></td></tr>
  </table>`;
  html += '</div>';

  document.getElementById('server-info').innerHTML = html;
}

tabInitializers.server = () => { loadServerInfo(); };

/* ── Init ──────────────────────────────────────────── */
document.addEventListener('DOMContentLoaded', () => {
  document.querySelectorAll('.sidebar-nav a').forEach(a => {
    a.addEventListener('click', e => {
      e.preventDefault();
      navigate(a.dataset.tab);
    });
  });

  const hash = window.location.hash.slice(1) || 'dashboard';
  navigate(hash);

  document.getElementById('dash-player-type')?.addEventListener('change', () => {
    const sel = document.getElementById('dash-player-type').value;
    const urlInput = document.getElementById('dash-url');
    if (urlInput && !urlInput.value) {
      if (sel === 'hls') urlInput.placeholder = '/应用/流名称/hls.m3u8';
      else if (sel === 'wsflv') urlInput.placeholder = 'ws://主机地址/应用/流名称.flv';
      else urlInput.placeholder = '/应用/流名称.flv';
    }
  });
});
