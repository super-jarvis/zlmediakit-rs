/* ── state ────────────────────────────────────────── */
let state = {
  apiSecret: '',
  authenticated: false,
  streams: [],
  proxies: [],
  recordings: {},
  forwarders: [],
  ffmpegSources: [],
  serverInfo: null,
  statistics: null,
  threadLoads: [],
  workThreadLoads: [],
  apiList: [],
  currentPlayer: null,
  refreshTimers: [],
  sessions: [],
  devices: [],
  catalog: [],
  talks: [],
  rtpServers: [],
  transcodes: [],
  objectUrls: [],
};

/* ── router ───────────────────────────────────────── */
function navigate(tab) {
  if (!state.authenticated) return;
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

class ApiError extends Error {
  constructor(message, status = 0, code = null) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

async function apiGet(endpoint, params = {}, options = {}) {
  const qs = Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null && v !== '')
    .map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`)
    .join('&');
  const url = `${API_BASE}/${endpoint}${qs ? '?' + qs : ''}`;
  const headers = {};
  if (state.apiSecret) headers['X-API-Secret'] = state.apiSecret;
  const res = await fetch(url, { headers, cache: 'no-store' });
  let data;
  try {
    data = await res.json();
  } catch (_) {
    throw new ApiError(`服务器返回了无效响应（HTTP ${res.status}）`, res.status);
  }
  if (res.status === 401) {
    if (!options.probe) lockConsole('登录已失效，请重新输入 API secret');
    throw new ApiError(data.msg || 'API secret 无效', 401, data.code);
  }
  if (!res.ok) throw new ApiError(data.msg || `HTTP ${res.status}`, res.status, data.code);
  if (typeof data.code === 'number' && data.code !== 0) {
    throw new ApiError(data.msg || `操作失败（code ${data.code}）`, res.status, data.code);
  }
  return data;
}

function lockConsole(message = '') {
  state.authenticated = false;
  document.getElementById('login-screen')?.classList.remove('hidden');
  const error = document.getElementById('login-error');
  if (error) {
    error.textContent = message;
    error.style.display = message ? 'block' : 'none';
  }
}

async function loginWithSecret(secret) {
  state.apiSecret = secret;
  await apiGet('getApiList', {}, { probe: true });
  state.authenticated = true;
  sessionStorage.setItem('zlmediakit-api-secret', secret);
  document.getElementById('login-screen')?.classList.add('hidden');
  navigate(window.location.hash.slice(1) || 'dashboard');
}

function logout() {
  stopPlayer();
  state.apiSecret = '';
  state.authenticated = false;
  sessionStorage.removeItem('zlmediakit-api-secret');
  const input = document.getElementById('login-secret');
  if (input) input.value = '';
  lockConsole();
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
    const form = document.getElementById('stream-filter');
    const data = await apiGet('getMediaList', form ? {
      vhost: '__defaultVhost__',
      app: form.app.value.trim(),
      stream: form.stream.value.trim(),
    } : {});
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
      <td><a href="#" onclick="setPlayerUrl(${jsString(app)},${jsString(stream)});return false;">${escHtml(stream)}</a></td>
      <td><span class="badge badge-green">直播中</span></td>
      <td>${s.readerCount || 0}</td>
      <td style="font-size:12px">${alive}</td>
      <td style="font-size:11px;max-width:280px;overflow:hidden;text-overflow:ellipsis;">${vinfo}</td>
      <td style="font-size:11px;max-width:200px;overflow:hidden;text-overflow:ellipsis;">${ainfo}</td>
      <td>
        <button class="btn btn-sm" onclick="showStreamDetail(${jsString(app)},${jsString(stream)})">详情</button>
        <button class="btn btn-sm" onclick="showSnapshot(${jsString(app)},${jsString(stream)})">截图</button>
        <button class="btn btn-danger btn-sm" onclick="closeStream(${jsString(app)},${jsString(stream)})">✕ 关闭</button>
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
  if (s === undefined || s === null) return '';
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

async function checkMediaOnline() {
  const form = document.getElementById('stream-filter');
  const app = form.app.value.trim() || 'live';
  const stream = form.stream.value.trim();
  if (!stream) {
    showAlert('dash-alerts', 'error', '在线探测需要填写流名称');
    return;
  }
  try {
    const data = await apiGet('isMediaOnline', { vhost: '__defaultVhost__', app, stream });
    showAlert('dash-alerts', data.online ? 'success' : 'error', `${app}/${stream} ${data.online ? '在线' : '离线'}`);
  } catch (e) { showAlert('dash-alerts', 'error', e.message); }
}

async function closeFilteredStreams() {
  const form = document.getElementById('stream-filter');
  const app = form.app.value.trim();
  const stream = form.stream.value.trim();
  if (!app && !stream) {
    showAlert('dash-alerts', 'error', '为避免关闭全部流，请至少填写应用名或流名称');
    return;
  }
  if (!confirm(`确认批量关闭 ${app || '*'} / ${stream || '*'} 的匹配流？`)) return;
  try {
    const data = await apiGet('close_streams', { vhost: '__defaultVhost__', app, stream });
    showAlert('dash-alerts', 'success', data.msg || `已关闭 ${data.count_closed || 0} 条流`);
    loadDashboard();
  } catch (e) { showAlert('dash-alerts', 'error', e.message); }
}

function jsString(value) {
  return escHtml(JSON.stringify(String(value ?? '')));
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

function apiUrl(endpoint, params = {}) {
  const qs = new URLSearchParams();
  Object.entries(params).forEach(([key, value]) => {
    if (value !== undefined && value !== null && value !== '') qs.set(key, value);
  });
  return `${API_BASE}/${endpoint}${qs.size ? '?' + qs.toString() : ''}`;
}

function openDetailModal(title, html) {
  document.getElementById('detail-title').textContent = title;
  document.getElementById('detail-content').innerHTML = html;
  const modal = document.getElementById('detail-modal');
  modal.classList.add('open');
  modal.setAttribute('aria-hidden', 'false');
}

function closeDetailModal() {
  const modal = document.getElementById('detail-modal');
  modal.classList.remove('open');
  modal.setAttribute('aria-hidden', 'true');
  state.objectUrls.forEach(url => URL.revokeObjectURL(url));
  state.objectUrls = [];
}

async function showStreamDetail(app, stream) {
  openDetailModal(`${app}/${stream}`, '<p class="text-secondary">加载中...</p>');
  try {
    const [info, players] = await Promise.all([
      apiGet('getMediaInfo', { vhost: '__defaultVhost__', app, stream }),
      apiGet('getMediaPlayerList', { vhost: '__defaultVhost__', app, stream }),
    ]);
    const data = info.result || {};
    const viewerRows = (players.data || []).map(p => `<tr><td>${escHtml(p.playerId)}</td><td>${escHtml(p.protocol || '-')}</td><td>${escHtml(p.peer_ip || '-')}</td><td>${fmtDuration(p.aliveSecond || 0)}</td></tr>`).join('');
    const base = `${window.location.hostname}`;
    const html = `<dl class="detail-grid">
      <div class="detail-item"><dt>VHost</dt><dd>${escHtml(data.vhost || '__defaultVhost__')}</dd></div>
      <div class="detail-item"><dt>观看人数</dt><dd>${data.readerCount || 0}</dd></div>
      <div class="detail-item"><dt>创建时间</dt><dd>${data.createTime ? new Date(data.createTime).toLocaleString() : '-'}</dd></div>
      <div class="detail-item"><dt>内部 URL</dt><dd>${escHtml(data.url || '-')}</dd></div>
    </dl>
    <h4>媒体轨道</h4><pre class="code-block">${escHtml(JSON.stringify(data.tracks || [], null, 2))}</pre>
    <h4>播放地址</h4><pre class="code-block">HTTP-FLV  ${location.protocol}//${base}${location.port ? ':' + location.port : ''}/${app}/${stream}.flv\nHLS       ${location.protocol}//${base}${location.port ? ':' + location.port : ''}/${app}/${stream}/hls.m3u8\nWHEP      ${location.protocol}//${base}/webrtc/play/${app}/${stream}</pre>
    <h4>播放器</h4><div class="table-wrap"><table><thead><tr><th>ID</th><th>协议</th><th>地址</th><th>在线</th></tr></thead><tbody>${viewerRows || '<tr class="empty-row"><td colspan="4">暂无播放器</td></tr>'}</tbody></table></div>`;
    openDetailModal(`${app}/${stream}`, html);
  } catch (e) {
    openDetailModal(`${app}/${stream}`, `<div class="alert alert-error">${escHtml(e.message)}</div>`);
  }
}

async function showSnapshot(app, stream) {
  openDetailModal(`${app}/${stream} 截图`, '<p class="text-secondary">正在生成截图...</p>');
  try {
    const res = await fetch(apiUrl('getSnap', { vhost: '__defaultVhost__', app, stream }), {
      headers: state.apiSecret ? { 'X-API-Secret': state.apiSecret } : {},
      cache: 'no-store',
    });
    if (res.status === 401) {
      lockConsole('登录已失效，请重新输入 API secret');
      throw new ApiError('API secret 无效', 401);
    }
    if (!res.ok) throw new ApiError(`截图失败（HTTP ${res.status}）`, res.status);
    const type = res.headers.get('content-type') || '';
    if (!type.startsWith('image/')) {
      const data = await res.json();
      throw new ApiError(data.msg || '截图失败', res.status, data.code);
    }
    const url = URL.createObjectURL(await res.blob());
    state.objectUrls.push(url);
    openDetailModal(`${app}/${stream} 截图`, `<img class="snapshot" src="${url}" alt="${escHtml(stream)} snapshot" />`);
  } catch (e) {
    openDetailModal(`${app}/${stream} 截图`, `<div class="alert alert-error">${escHtml(e.message)}</div>`);
  }
}

async function setPlayerUrl(app, stream) {
  const sel = document.getElementById('dash-player-type');
  const port = window.location.port ? ':' + window.location.port : '';
  const host = window.location.hostname + port;
  const wsScheme = window.location.protocol === 'https:' ? 'wss' : 'ws';
  switch (sel.value) {
    case 'hls':
      document.getElementById('dash-url').value = `/${app}/${stream}/hls.m3u8`;
      break;
    case 'wsflv':
      document.getElementById('dash-url').value = `${wsScheme}://${host}/${app}/${stream}.flv`;
      break;
    case 'whep': {
      try {
        const response = state.serverInfo || await apiGet('getServerConfig');
        state.serverInfo = response;
        const webRtcPort = response.result?.webrtc?.port || '9000';
        const whepQuery = new URLSearchParams({ vhost: '__defaultVhost__', app, stream });
        document.getElementById('dash-url').value =
          `http://${window.location.hostname}:${webRtcPort}/webrtc/play?${whepQuery.toString()}`;
      } catch (e) {
        showAlert('dash-alerts', 'error', `无法读取 WebRTC 端口: ${e.message}`);
        return;
      }
      break;
    }
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

  const isWs = url.startsWith('ws://') || url.startsWith('wss://');
  const isAbs = url.startsWith('http://') || url.startsWith('https://') || isWs;
  const host = window.location.host;
  const proto = window.location.protocol === 'https:' ? 'https' : 'http';
  const fullUrl = isAbs ? url : proto + '://' + host + url;

  if (playerType === 'whep' || url.includes('/webrtc/play/')) {
    info.textContent = 'WebRTC (WHEP): ' + fullUrl;
    startWhepPlayer(video, fullUrl, errEl).catch(e => {
      errEl.textContent = `WebRTC 播放失败: ${e.message}`;
      errEl.style.display = 'block';
    });
  } else if (url.endsWith('.m3u8') || playerType === 'hls') {
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
  } else if (isWs || url.endsWith('.flv') || playerType === 'flv' || playerType === 'wsflv') {
    const label = isWs ? 'WebSocket-FLV' : 'HTTP-FLV';
    info.textContent = label + ': ' + (isWs ? url : fullUrl);
    if (typeof flvjs !== 'undefined' && flvjs.isSupported()) {
      const f = flvjs.createPlayer(
        { type: 'flv', url: isWs ? url : fullUrl, isLive: true },
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

async function waitForIceGathering(pc, timeoutMs = 4000) {
  if (pc.iceGatheringState === 'complete') return;
  await new Promise(resolve => {
    const timer = setTimeout(done, timeoutMs);
    function done() {
      clearTimeout(timer);
      pc.removeEventListener('icegatheringstatechange', onStateChange);
      resolve();
    }
    function onStateChange() {
      if (pc.iceGatheringState === 'complete') done();
    }
    pc.addEventListener('icegatheringstatechange', onStateChange);
  });
}

async function startWhepPlayer(video, endpoint, errEl) {
  if (!window.RTCPeerConnection) throw new Error('当前浏览器不支持 WebRTC');
  if (window.location.protocol === 'https:' && endpoint.startsWith('http:')) {
    throw new Error('HTTPS 页面不能访问 HTTP WHEP 端口，请为 WebRTC 配置 HTTPS 反向代理');
  }

  const pc = new RTCPeerConnection();
  pc.addTransceiver('video', { direction: 'recvonly' });
  pc.addTransceiver('audio', { direction: 'recvonly' });
  pc.addEventListener('track', event => {
    const stream = event.streams[0] || new MediaStream([event.track]);
    if (video.srcObject !== stream) video.srcObject = stream;
    video.play().catch(() => {});
  });
  pc.addEventListener('connectionstatechange', () => {
    if (pc.connectionState === 'failed') {
      errEl.textContent = 'WebRTC 连接失败，请检查 ICE/STUN 配置和媒体编码';
      errEl.style.display = 'block';
    }
  });

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  await waitForIceGathering(pc);
  const response = await fetch(endpoint, {
    method: 'POST',
    headers: { 'Content-Type': 'application/sdp' },
    body: pc.localDescription.sdp,
  });
  if (!response.ok) {
    pc.close();
    throw new Error(`${response.status} ${await response.text()}`.trim());
  }
  const answer = await response.text();
  await pc.setRemoteDescription({ type: 'answer', sdp: answer });
  const resourceUrl = response.headers.get('Location');
  state.currentPlayer = {
    close() {
      pc.close();
      if (resourceUrl) {
        const target = new URL(resourceUrl, endpoint).toString();
        fetch(target, { method: 'DELETE', keepalive: true }).catch(() => {});
      }
    },
  };
}

function stopPlayer() {
  const video = document.getElementById('dash-video');
  if (state.currentPlayer) {
    if (state.currentPlayer.destroy) state.currentPlayer.destroy();
    else if (state.currentPlayer.close) state.currentPlayer.close();
    else if (state.currentPlayer.pause) { state.currentPlayer.pause(); video.src = ''; }
    state.currentPlayer = null;
  }
  if (video) {
    video.pause();
    video.srcObject = null;
    video.removeAttribute('src');
  }
  const c = document.getElementById('dash-player-container');
  if (c) c.style.display = 'none';
}

/* ── Sessions ─────────────────────────────────────── */
async function loadSessions() {
  const form = document.getElementById('session-filter');
  const params = form ? {
    peer_ip: form.peer_ip.value.trim(),
    local_port: form.local_port.value.trim(),
  } : {};
  try {
    const data = await apiGet('getAllSession', params);
    state.sessions = data.data || [];
    const tb = document.getElementById('session-table-body');
    tb.innerHTML = state.sessions.map(s => `<tr>
      <td><span class="badge badge-blue">${escHtml(s.typeid || '-')}</span></td>
      <td>${escHtml(s.peer_ip || '-')}:${s.peer_port || 0}</td>
      <td>${escHtml(s.local_ip || '-')}:${s.local_port || 0}</td>
      <td>${escHtml(s.stream || '-')}</td>
      <td>${s.created_at ? new Date(s.created_at * 1000).toLocaleString() : '-'}</td>
      <td><button class="btn btn-danger btn-sm" onclick="kickSession(${jsString(s.id)})">断开</button></td>
    </tr>`).join('') || '<tr class="empty-row"><td colspan="6">暂无活动会话</td></tr>';
  } catch (e) {
    document.getElementById('session-table-body').innerHTML = `<tr class="empty-row"><td colspan="6">加载失败: ${escHtml(e.message)}</td></tr>`;
  }
}

async function kickSession(id) {
  if (!confirm('确认断开该会话？')) return;
  try {
    await apiGet('kick_session', { id });
    showAlert('session-alerts', 'success', '会话已断开');
    loadSessions();
  } catch (e) { showAlert('session-alerts', 'error', e.message); }
}

async function kickFilteredSessions() {
  const form = document.getElementById('session-filter');
  const params = {
    peer_ip: form.peer_ip.value.trim(),
    local_port: form.local_port.value.trim(),
    typeid: form.typeid.value.trim(),
  };
  if (!params.peer_ip && !params.local_port && !params.typeid) {
    showAlert('session-alerts', 'error', '为避免断开全部连接，请至少填写一个筛选条件');
    return;
  }
  if (!confirm('确认断开所有符合筛选条件的会话？')) return;
  try {
    const data = await apiGet('kick_sessions', params);
    showAlert('session-alerts', 'success', data.msg || `已断开 ${data.count_hit || 0} 个会话`);
    loadSessions();
  } catch (e) { showAlert('session-alerts', 'error', e.message); }
}

tabInitializers.sessions = () => { loadSessions(); };

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
      <td><button class="btn btn-danger btn-sm" onclick="deleteProxy(${jsString(app)},${jsString(stream)})">✕ 删除</button></td>
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
        ${!hlsOn ? `<button class="btn btn-sm" onclick="startRec(${jsString(app)},${jsString(stream)},'hls')">HLS</button>` : ''}
        ${!flvOn ? `<button class="btn btn-sm" onclick="startRec(${jsString(app)},${jsString(stream)},'flv')">FLV</button>` : ''}
        ${!mp4On ? `<button class="btn btn-sm" onclick="startRec(${jsString(app)},${jsString(stream)},'mp4')">MP4</button>` : ''}
      </td>
      <td>
        <button class="btn btn-sm" onclick="showRecordStatus(${jsString(app)},${jsString(stream)})">详情</button>
        <button class="btn btn-danger btn-sm" onclick="stopRec(${jsString(app)},${jsString(stream)})">停止全部</button>
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

async function showRecordStatus(app, stream) {
  openDetailModal(`${app}/${stream} 录制状态`, '<p class="text-secondary">加载中...</p>');
  try {
    const data = await apiGet('getRecordStatus', { vhost: '__defaultVhost__', app, stream });
    openDetailModal(`${app}/${stream} 录制状态`, `<pre class="code-block">${escHtml(JSON.stringify(data.recording || {}, null, 2))}</pre>`);
  } catch (e) { openDetailModal(`${app}/${stream} 录制状态`, `<div class="alert alert-error">${escHtml(e.message)}</div>`); }
}

function fmtBytes(bytes) {
  const n = Number(bytes) || 0;
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MiB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GiB`;
}

async function loadRecordFiles() {
  const form = document.getElementById('record-file-form');
  const app = form.app.value.trim() || 'live';
  const stream = form.stream.value.trim();
  if (!stream) return;
  try {
    const data = await apiGet('getMp4RecordFile', {
      vhost: '__defaultVhost__', app, stream, period: form.period.value,
    });
    const files = data.data || [];
    document.getElementById('record-file-table-body').innerHTML = files.map(f => `<tr>
      <td>${escHtml(f.file_name)}</td>
      <td>${f.startTime ? new Date(f.startTime).toLocaleString() : '-'}</td>
      <td>${fmtBytes(f.file_size)}</td>
      <td class="actions"><button class="btn btn-sm" onclick="openRecordFile(${jsString(f.url)},${jsString(app)},${jsString(stream)},${jsString(f.file_name)})">播放/下载</button></td>
    </tr>`).join('') || '<tr class="empty-row"><td colspan="4">没有匹配的 MP4 录像</td></tr>';
  } catch (e) {
    document.getElementById('record-file-table-body').innerHTML = `<tr class="empty-row"><td colspan="4">查询失败: ${escHtml(e.message)}</td></tr>`;
  }
}

async function sha256Hex(value) {
  if (!window.crypto?.subtle) return '';
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(value));
  return Array.from(new Uint8Array(digest), b => b.toString(16).padStart(2, '0')).join('');
}

async function openRecordFile(url, app, stream, fileName) {
  const streamPath = `${app}/${stream}/${fileName}`;
  const sign = await sha256Hex(`${state.apiSecret}|__defaultVhost__|record|${streamPath}|play`);
  const target = `${url}${url.includes('?') ? '&' : '?'}sign=${encodeURIComponent(sign)}`;
  window.open(target, '_blank', 'noopener');
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
      <td><button class="btn btn-danger btn-sm" onclick="deleteForwarder(${jsString(app)},${jsString(stream)})">✕ 停止</button></td>
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
      <td><button class="btn btn-danger btn-sm" onclick="deleteFFmpeg(${jsString(app)},${jsString(stream)})">✕ 删除</button></td>
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

/* ── GB28181 ──────────────────────────────────────── */
async function loadGb28181() {
  try {
    const [devices, sip, talks] = await Promise.all([
      apiGet('getDeviceList'),
      apiGet('getSipInfo'),
      apiGet('getTalkList'),
    ]);
    state.devices = devices.data || [];
    state.talks = talks.data || [];
    const sipInfo = sip.result || {};
    document.getElementById('sip-summary').innerHTML = `
      <div class="stat-item"><span class="stat-value">${state.devices.length}</span><span class="stat-label">注册设备</span></div>
      <div class="stat-item"><span class="stat-value">${state.devices.filter(d => d.online).length}</span><span class="stat-label">在线设备</span></div>
      <div class="stat-item"><span class="stat-value">${state.talks.length}</span><span class="stat-label">活动对讲</span></div>
      <div class="stat-item"><span class="stat-value">${escHtml(sipInfo.port || sipInfo.sip_port || '-')}</span><span class="stat-label">SIP 端口</span></div>`;
    document.getElementById('device-table-body').innerHTML = state.devices.map(d => `<tr>
      <td><strong>${escHtml(d.device_id)}</strong><br><span class="text-secondary text-small">${escHtml(d.name || d.manufacturer || '')}</span></td>
      <td>${escHtml(d.ip)}:${d.port || 0}</td>
      <td><span class="badge ${d.online ? 'badge-green' : 'badge-gray'}">${d.online ? '在线' : '离线'}</span></td>
      <td>${fmtDuration((d.last_seen_ms || 0) / 1000)}前<br><span class="text-secondary text-small">有效期 ${fmtDuration((d.expires_ms || 0) / 1000)}</span></td>
      <td class="actions"><button class="btn btn-sm" onclick="queryCatalog(${jsString(d.device_id)})">目录</button><button class="btn btn-sm" onclick="showDeviceInfo(${jsString(d.device_id)})">详情</button></td>
    </tr>`).join('') || '<tr class="empty-row"><td colspan="5">暂无已注册设备</td></tr>';
    renderTalks();
  } catch (e) {
    document.getElementById('device-table-body').innerHTML = `<tr class="empty-row"><td colspan="5">${escHtml(e.message)}</td></tr>`;
    document.getElementById('sip-summary').innerHTML = '';
  }
}

function renderTalks() {
  document.getElementById('talk-table-body').innerHTML = state.talks.map(t => `<tr>
    <td><strong>${escHtml(t.device_id)}</strong><br><span class="text-secondary text-small">${escHtml(t.channel_id)}</span></td>
    <td>${escHtml(t.vhost)}/${escHtml(t.app)}/${escHtml(t.stream)}</td>
    <td>${escHtml(t.codec)}</td><td>${t.ssrc}</td><td>${t.local_port}</td>
    <td class="actions"><button class="btn btn-danger btn-sm" onclick="stopGbTalk(${jsString(t.channel_id)})">停止对讲</button></td>
  </tr>`).join('') || '<tr class="empty-row"><td colspan="6">暂无活动对讲</td></tr>';
}

function renderCatalog(deviceId, channels) {
  state.catalog = channels || [];
  document.getElementById('catalog-title').textContent = `${deviceId} · ${state.catalog.length} 个通道`;
  document.getElementById('catalog-table-body').innerHTML = state.catalog.map(c => `<tr>
    <td>${escHtml(c.channel_id)}</td><td>${escHtml(c.name || '-')}</td>
    <td><span class="badge ${String(c.status).toUpperCase() === 'ON' ? 'badge-green' : 'badge-gray'}">${escHtml(c.status || '-')}</span></td>
    <td>${escHtml([c.manufacturer, c.model].filter(Boolean).join(' / ') || '-')}</td>
    <td class="actions"><button class="btn btn-primary btn-sm" onclick="startGbStream(${jsString(deviceId)},${jsString(c.channel_id)})">点播</button><button class="btn btn-sm" onclick="startGbTalk(${jsString(deviceId)},${jsString(c.channel_id)})">对讲</button><button class="btn btn-danger btn-sm" onclick="stopGbStream(${jsString(c.channel_id)})">停止点播</button><button class="btn btn-danger btn-sm" onclick="stopGbTalk(${jsString(c.channel_id)})">停止对讲</button></td>
  </tr>`).join('') || '<tr class="empty-row"><td colspan="5">设备未返回通道</td></tr>';
}

async function queryCatalog(deviceId) {
  document.getElementById('catalog-title').textContent = `${deviceId} · 正在查询...`;
  try {
    const data = await apiGet('queryCatalog', { device_id: deviceId });
    renderCatalog(deviceId, data.result || []);
  } catch (e) { showAlert('gb-alerts', 'error', e.message); }
}

async function showDeviceInfo(deviceId) {
  openDetailModal(`设备 ${deviceId}`, '<p class="text-secondary">加载中...</p>');
  const [cached, queried] = await Promise.allSettled([
    apiGet('getDeviceInfo', { device_id: deviceId }),
    apiGet('queryDeviceInfo', { device_id: deviceId }),
  ]);
  if (cached.status === 'rejected' && queried.status === 'rejected') {
    openDetailModal(`设备 ${deviceId}`, `<div class="alert alert-error">${escHtml(queried.reason?.message || cached.reason?.message)}</div>`);
    return;
  }
  const cachedInfo = cached.status === 'fulfilled' ? cached.value.result : null;
  const queriedInfo = queried.status === 'fulfilled' ? queried.value.result : null;
  openDetailModal(`设备 ${deviceId}`, `<h4>注册缓存</h4><pre class="code-block">${escHtml(JSON.stringify(cachedInfo || {}, null, 2))}</pre><h4>实时查询</h4><pre class="code-block">${escHtml(JSON.stringify(queriedInfo || {}, null, 2))}</pre>`);
}

async function stopSipServer() {
  if (!confirm('停止 SIP 服务会清空设备并关闭相关媒体流，且需要重启服务器才能恢复。确认继续？')) return;
  try {
    await apiGet('stopSip');
    showAlert('gb-alerts', 'success', 'SIP 服务已停止');
    loadGb28181();
  } catch (e) { showAlert('gb-alerts', 'error', e.message); }
}

async function startGbStream(deviceId, channelId) {
  try {
    const data = await apiGet('startRtp', { device_id: deviceId, channel_id: channelId });
    showAlert('gb-alerts', 'success', `点播已发起，RTP 端口 ${data.result}`);
    setTimeout(loadRtpServers, 500);
  } catch (e) { showAlert('gb-alerts', 'error', e.message); }
}

async function stopGbStream(channelId) {
  if (!confirm(`确认停止通道 ${channelId}？`)) return;
  try {
    await apiGet('stopRtp', { app: 'gb28181', channel_id: channelId });
    showAlert('gb-alerts', 'success', '点播已停止');
  } catch (e) { showAlert('gb-alerts', 'error', e.message); }
}

async function startGbTalk(deviceId, channelId) {
  const vhost = document.getElementById('talk-vhost').value.trim() || '__defaultVhost__';
  const app = document.getElementById('talk-app').value.trim() || 'live';
  const stream = document.getElementById('talk-stream').value.trim();
  if (!stream) {
    showAlert('gb-alerts', 'error', '请先填写已发布的 G.711A/G.711U 音源流名称');
    document.getElementById('talk-stream').focus();
    return;
  }
  try {
    const data = await apiGet('startTalk', { device_id: deviceId, channel_id: channelId, vhost, app, stream });
    showAlert('gb-alerts', 'success', `语音对讲已发起，本地 RTP 端口 ${data.local_port}`);
    setTimeout(loadGb28181, 500);
  } catch (e) { showAlert('gb-alerts', 'error', `发起对讲失败: ${e.message}`); }
}

async function stopGbTalk(channelId) {
  if (!confirm(`确认停止通道 ${channelId} 的语音对讲？`)) return;
  try {
    const data = await apiGet('stopTalk', { channel_id: channelId });
    showAlert('gb-alerts', data.stopped ? 'success' : 'error', data.stopped ? '语音对讲已停止' : '未找到活动对讲');
    loadGb28181();
  } catch (e) { showAlert('gb-alerts', 'error', `停止对讲失败: ${e.message}`); }
}

tabInitializers.gb28181 = () => { loadGb28181(); };

/* ── RTP servers ──────────────────────────────────── */
async function loadRtpServers() {
  try {
    const data = await apiGet('listRtpServer');
    state.rtpServers = data.result || [];
    document.getElementById('rtp-table-body').innerHTML = state.rtpServers.map(r => `<tr>
      <td>${r.port}</td><td>${escHtml(r.app)}/${escHtml(r.stream)}</td><td>${escHtml(r.payload_type)}</td>
      <td>${r.ssrc ?? '-'}</td><td>${fmtBytes(r.bytes)} · ${r.packets || 0} 包</td>
      <td class="actions"><button class="btn btn-sm" onclick="showRtpInfo(${Number(r.port)})">详情</button><button class="btn btn-danger btn-sm" onclick="closeRtpServer(${Number(r.port)})">关闭</button></td>
    </tr>`).join('') || '<tr class="empty-row"><td colspan="6">暂无 RTP 接收端口</td></tr>';
  } catch (e) {
    document.getElementById('rtp-table-body').innerHTML = `<tr class="empty-row"><td colspan="6">${escHtml(e.message)}</td></tr>`;
  }
}

async function openRtpServer() {
  const form = document.getElementById('rtp-form');
  try {
    const data = await apiGet('openRtpServer', {
      port: form.port.value, app: form.app.value.trim(), stream: form.stream.value.trim(),
      type: form.type.value, ssrc: form.ssrc.value,
    });
    showAlert('rtp-alerts', 'success', `RTP 端口已打开：${data.result?.port ?? data.result}`);
    loadRtpServers();
  } catch (e) { showAlert('rtp-alerts', 'error', e.message); }
}

async function closeRtpServer(port) {
  if (!confirm(`确认关闭 RTP 端口 ${port}？`)) return;
  try { await apiGet('closeRtpServer', { port }); loadRtpServers(); }
  catch (e) { showAlert('rtp-alerts', 'error', e.message); }
}

async function showRtpInfo(port) {
  openDetailModal(`RTP 端口 ${port}`, '<p class="text-secondary">加载中...</p>');
  try {
    const data = await apiGet('getRtpInfo', { port });
    openDetailModal(`RTP 端口 ${port}`, `<pre class="code-block">${escHtml(JSON.stringify(data.result || {}, null, 2))}</pre>`);
  } catch (e) { openDetailModal(`RTP 端口 ${port}`, `<div class="alert alert-error">${escHtml(e.message)}</div>`); }
}

tabInitializers.rtp = () => { loadRtpServers(); };

/* ── Transcode ────────────────────────────────────── */
async function loadTranscodes() {
  try {
    const data = await apiGet('getTranscodeList');
    state.transcodes = data.result || [];
    document.getElementById('transcode-table-body').innerHTML = state.transcodes.map(t => `<tr>
      <td><code>${escHtml(t.key)}</code></td><td>${escHtml(t.app)}/${escHtml(t.stream)}</td>
      <td>${escHtml(t.dst_app)}/${escHtml(t.dst_stream)}</td><td>${escHtml(t.input_codec || '-')} → ${escHtml(t.output_codec || '-')}</td>
      <td>${t.in_frames || 0} / ${t.out_frames || 0} 帧</td>
      <td class="actions"><button class="btn btn-sm" onclick="showTranscode(${jsString(t.key)})">详情</button><button class="btn btn-danger btn-sm" onclick="deleteTranscode(${jsString(t.key)})">停止</button></td>
    </tr>`).join('') || '<tr class="empty-row"><td colspan="6">暂无转码任务</td></tr>';
  } catch (e) {
    document.getElementById('transcode-table-body').innerHTML = `<tr class="empty-row"><td colspan="6">${escHtml(e.message)}</td></tr>`;
  }
}

async function addTranscode() {
  const form = document.getElementById('transcode-form');
  try {
    const data = await apiGet('addTranscode', {
      vhost: '__defaultVhost__', app: form.app.value.trim() || 'live', stream: form.stream.value.trim(),
      code: form.code.value, scale: form.scale.value.trim(), bitrate: form.bitrate.value.trim(),
      name: form.name.value.trim(), dst_app: form.dst_app.value.trim(), dst_stream: form.dst_stream.value.trim(),
    });
    showAlert('transcode-alerts', 'success', `任务已创建：${data.result?.key || ''}`);
    loadTranscodes();
  } catch (e) { showAlert('transcode-alerts', 'error', e.message); }
}

async function deleteTranscode(key) {
  if (!confirm('确认停止该转码任务？')) return;
  try { await apiGet('delTranscode', { key }); loadTranscodes(); }
  catch (e) { showAlert('transcode-alerts', 'error', e.message); }
}

async function showTranscode(key) {
  openDetailModal(`转码任务 ${key}`, '<p class="text-secondary">加载中...</p>');
  try {
    const data = await apiGet('getTranscode', { key });
    openDetailModal(`转码任务 ${key}`, `<pre class="code-block">${escHtml(JSON.stringify(data.result || data, null, 2))}</pre>`);
  } catch (e) { openDetailModal(`转码任务 ${key}`, `<div class="alert alert-error">${escHtml(e.message)}</div>`); }
}

tabInitializers.transcode = () => { loadTranscodes(); };

/* ── Server ────────────────────────────────────────── */
async function loadServerInfo() {
  try {
    const [cfg, stat, threads, workers, apis] = await Promise.all([
      apiGet('getServerConfig'),
      apiGet('getStatistic'),
      apiGet('getThreadsLoad'),
      apiGet('getWorkThreadsLoad'),
      apiGet('getApiList'),
    ]);
    state.serverInfo = cfg;
    state.statistics = stat.result?.data || [];
    state.threadLoads = threads.data || [];
    state.workThreadLoads = workers.data || [];
    state.apiList = apis.data || [];
    renderServerPage();
  } catch (e) {
    document.getElementById('server-info').innerHTML =
      `<div class="alert alert-error">加载失败: ${e.message}</div>`;
  }
}

function renderServerPage() {
  const response = state.serverInfo || {};
  const info = response.result || {};
  const stats = state.statistics;
  const entries = info.data || [];
  let html = `<div class="status-bar">
    <div class="stat-item"><span class="stat-value">${escHtml(response.version || '-')}</span><span class="stat-label">版本</span></div>
    <div class="stat-item"><span class="stat-value">${stats.length}</span><span class="stat-label">活动流</span></div>
    <div class="stat-item"><span class="stat-value">${state.threadLoads.length}</span><span class="stat-label">工作线程</span></div>
  </div>`;

  html += `<div class="card"><div class="card-header"><h3>运行时配置</h3><div><span class="text-secondary text-small">修改仅作用于当前进程</span> <button type="button" class="btn btn-sm" onclick="reloadCertificates()">重载 TLS 证书</button></div></div>
    <form id="config-form" onsubmit="event.preventDefault();updateServerConfig();"><div class="form-row">
      <div class="form-group"><label>配置键</label><input class="code-input" name="key" list="config-keys" required placeholder="general.flowThreshold" /></div>
      <div class="form-group"><label>新值</label><input class="code-input" name="value" required /></div>
      <div class="form-group form-action"><label>&nbsp;</label><button class="btn btn-primary">应用配置</button></div>
    </div><datalist id="config-keys">${entries.filter(e => !String(e.key).toLowerCase().includes('secret')).map(e => `<option value="${escHtml(e.key)}">`).join('')}</datalist></form>
    <div class="table-wrap"><table><thead><tr><th>配置键</th><th>当前值</th><th>操作</th></tr></thead><tbody>${entries.map(e => `<tr><td><code>${escHtml(e.key)}</code></td><td>${escHtml(e.value)}</td><td>${String(e.key).toLowerCase().includes('secret') ? '-' : `<button class="btn btn-sm" onclick="selectConfigKey(${jsString(e.key)},${jsString(e.value)})">编辑</button>`}</td></tr>`).join('')}</tbody></table></div></div>`;

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

  const threadRows = [...state.threadLoads.map(t => ({ ...t, group: '事件线程' })), ...state.workThreadLoads.map(t => ({ ...t, group: '工作线程' }))];
  html += `<div class="card"><div class="card-header"><h3>线程负载</h3><button class="btn btn-sm" onclick="loadServerInfo()">↻ 刷新</button></div><div class="table-wrap"><table><thead><tr><th>类型</th><th>线程</th><th>负载</th><th>延迟</th></tr></thead><tbody>${threadRows.map(t => `<tr><td>${t.group}</td><td>${t.thread_id}</td><td>${t.load}%</td><td>${t.delay} ms</td></tr>`).join('') || '<tr class="empty-row"><td colspan="4">暂无数据</td></tr>'}</tbody></table></div></div>`;

  html += `<div class="card"><div class="card-header"><h3>API 能力</h3><span>${state.apiList.length} 个接口</span></div><div class="code-block">${state.apiList.map(escHtml).join('\n')}</div></div>`;

  html += '<div class="card"><div class="card-header"><h3>流地址参考</h3></div>';
  const port = window.location.port ? ':' + window.location.port : '';
  const host = window.location.hostname + port;
  const rtmpPort = info.rtmp?.port || '1935';
  const rtspPort = info.rtsp?.port || '8554';
  const webRtcPort = info.webrtc?.port || '9000';
  const srtPort = info.srt?.port || '9000';
  html += `<table>
    <tr><th>协议</th><th>地址</th></tr>
    <tr><td>RTMP</td><td><code>rtmp://${window.location.hostname}:${rtmpPort}/live/stream</code></td></tr>
    <tr><td>RTSP</td><td><code>rtsp://${window.location.hostname}:${rtspPort}/live/stream</code></td></tr>
    <tr><td>HTTP-FLV</td><td><code>http://${host}/live/stream.flv</code></td></tr>
    <tr><td>HLS</td><td><code>http://${host}/live/stream/hls.m3u8</code></td></tr>
    <tr><td>WebSocket-FLV</td><td><code>${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${host}/live/stream.flv</code></td></tr>
    <tr><td>WebRTC 播放</td><td><code>http://${window.location.hostname}:${webRtcPort}/webrtc/play/__defaultVhost__/live/stream</code></td></tr>
    <tr><td>WebRTC 推流</td><td><code>http://${window.location.hostname}:${webRtcPort}/webrtc/publish/__defaultVhost__/live/stream</code></td></tr>
    <tr><td>SRT 推流</td><td><code>srt://${window.location.hostname}:${srtPort}?streamid=live/stream</code></td></tr>
    <tr><td>API 接口</td><td><code>http://${host}/index/api/getMediaList</code></td></tr>
  </table>`;
  html += '</div>';

  html += `<div class="card danger-zone"><div class="card-header"><h3>危险操作</h3></div><p class="text-secondary">重启会先返回成功，然后请求进程优雅退出；需要外部 supervisor 拉起服务。</p><br><button class="btn btn-danger" onclick="restartServer()">重启服务器</button></div>`;

  document.getElementById('server-info').innerHTML = html;
}

function selectConfigKey(key, value) {
  const form = document.getElementById('config-form');
  form.key.value = key;
  form.value.value = value;
  form.value.focus();
}

async function updateServerConfig() {
  const form = document.getElementById('config-form');
  const key = form.key.value.trim();
  const value = form.value.value.trim();
  if (!key || key.toLowerCase().includes('secret')) {
    alert('不能从管理页读取或修改 secret');
    return;
  }
  try {
    const result = await apiGet('setServerConfig', { [key]: value });
    const rejected = Array.isArray(result.rejected) ? result.rejected : [];
    if (rejected.length) {
      alert(`配置项不受支持：${rejected.join(', ')}`);
      return;
    }
    await loadServerInfo();
    const restartRequired = Array.isArray(result.restartRequired)
      ? result.restartRequired
      : [];
    if (restartRequired.length) {
      alert(`配置已保存到运行时快照，但以下项目需重启服务后生效：${restartRequired.join(', ')}`);
    }
  } catch (e) { alert(e.message); }
}

async function reloadCertificates() {
  if (!confirm('确认从配置的 PEM 文件重新加载所有 TLS 证书？现有连接不会中断。')) return;
  try {
    const result = await apiGet('reloadCertificate');
    const count = Number(result.reloaded ?? 0);
    alert(`TLS 证书已热加载，更新 ${count} 个监听器；新连接将使用新证书。`);
  } catch (e) {
    alert(`TLS 证书重载失败，服务仍保留最后一份有效证书：${e.message}`);
  }
}

async function restartServer() {
  if (!confirm('确认请求服务器优雅重启？请确保外部 supervisor 会重新拉起进程。')) return;
  try {
    await apiGet('restartServer');
    alert('重启请求已发送，页面将在服务恢复后重新登录。');
    logout();
  } catch (e) { alert(e.message); }
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

  document.getElementById('login-form')?.addEventListener('submit', async e => {
    e.preventDefault();
    const button = document.getElementById('login-button');
    const error = document.getElementById('login-error');
    button.disabled = true;
    error.style.display = 'none';
    try {
      await loginWithSecret(document.getElementById('login-secret').value);
    } catch (err) {
      lockConsole(err.message || '登录失败');
    } finally {
      button.disabled = false;
    }
  });
  document.getElementById('logout-button')?.addEventListener('click', logout);
  document.getElementById('detail-modal')?.addEventListener('click', e => {
    if (e.target.id === 'detail-modal') closeDetailModal();
  });

  const storedSecret = sessionStorage.getItem('zlmediakit-api-secret');
  if (storedSecret !== null) {
    document.getElementById('login-secret').value = storedSecret;
    loginWithSecret(storedSecret).catch(err => lockConsole(err.message || '登录已失效'));
  } else {
    lockConsole();
  }

  document.getElementById('dash-player-type')?.addEventListener('change', () => {
    const sel = document.getElementById('dash-player-type').value;
    const urlInput = document.getElementById('dash-url');
    if (urlInput && !urlInput.value) {
      if (sel === 'hls') urlInput.placeholder = '/应用/流名称/hls.m3u8';
      else if (sel === 'wsflv') urlInput.placeholder = `${location.protocol === 'https:' ? 'wss' : 'ws'}://主机地址/应用/流名称.flv`;
      else if (sel === 'whep') urlInput.placeholder = 'http://主机地址:WebRTC端口/webrtc/play/虚拟主机/应用/流名称';
      else urlInput.placeholder = '/应用/流名称.flv';
    }
  });
});
