/* indexd console.
   Vanilla JS, no build step, no frameworks, no external requests.

   Contract:
     GET  /api/info                  -> {intern_url, injecting}
     GET  /api/commands?limit=100    -> {"commands":[Command, ...]}  newest first
     POST /api/injection {enabled}   -> {"injecting": bool}
     GET  /api/events                -> SSE
        {"type":"command","command":{...}}   upsert by id
        {"type":"injection","enabled":bool}  the breaker moved somewhere else

   Command = {id, text, status, reply, error, created_at, started_at, finished_at,
               project_id, turn_id, project_url}
     status      queued | running | done | timed_out | failed | held
     timestamps  unix seconds; reply/error/started_at/finished_at may be null

   `held` means the request was recorded and deliberately not typed, because
   the breaker was open. It is not an error and is never rendered as one.

   Every string that comes off the wire (text, reply, error, id, status) is a
   voice transcript or raw terminal output and is therefore untrusted. It
   reaches the DOM only through textContent / createElement — there is no
   innerHTML in this file. */

(function () {
  'use strict';

  var MAX_ENTRIES = 100;
  var RECONNECT_MAX = 15000;

  var reduceMotion = window.matchMedia
    ? window.matchMedia('(prefers-reduced-motion: reduce)').matches
    : false;

  var el = {
    entries: document.getElementById('entries'),
    empty: document.getElementById('empty'),
    notice: document.getElementById('notice'),
    dot: document.getElementById('dot'),
    feedWord: document.getElementById('link-word'),
    host: document.getElementById('host'),
    port: document.getElementById('port'),
    internUrl: document.getElementById('intern'),
    announcer: document.getElementById('announcer'),
    breaker: document.getElementById('breaker'),
    breakerWord: document.getElementById('breaker-word'),
    breakerSub: document.getElementById('breaker-sub'),
    breakerMsg: document.getElementById('breaker-msg'),
    chipWord: document.getElementById('chip-word'),
    logCount: document.getElementById('log-count'),
    favicon: document.getElementById('favicon'),
    themeColor: document.getElementById('theme-color')
  };

  var records = new Map();   // id -> record
  var order = [];            // ids, newest first
  var seeded = false;        // first successful /api/commands has landed
  var connected = true;      // optimistic: only flips on a real SSE failure
  var openedOnce = false;
  var es = null;
  var backoff = 1000;
  var reconnectTimer = 0;
  var retryLoadTimer = 0;
  var ticker = 0;

  var injecting = null;      // true | false | null (not yet known)
  var postSeq = 0;
  /* What the log calls the far end. Not the URL: the URL is loopback and
     means nothing to a reader, while "intern" is the thing they actually
     know. The masthead shows the URL for when it does matter. */
  var DESTINATION = 'intern';

  /* ── formatting ─────────────────────────────────────────────────────── */

  var STATUS_WORDS = {
    queued: 'queued',
    running: 'running',
    done: 'done',
    timed_out: 'timed out',
    failed: 'failed',
    held: 'held',
    cancelled: 'stopped'
  };

  function statusWord(status) {
    return STATUS_WORDS[status] || status;
  }

  function num(v) {
    return (typeof v === 'number' && isFinite(v)) ? v : null;
  }

  function clock(sec) {
    if (sec === null) return '--:--:--';
    var d = new Date(sec * 1000);
    if (isNaN(d.getTime())) return '--:--:--';
    return d.toLocaleTimeString([], {
      hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit'
    });
  }

  function isoStamp(sec) {
    if (sec === null) return '';
    var d = new Date(sec * 1000);
    return isNaN(d.getTime()) ? '' : d.toISOString();
  }

  function dur(seconds) {
    var s = seconds > 0 ? seconds : 0;
    return (s < 100 ? s.toFixed(1) : String(Math.round(s))) + 's';
  }

  function durationText(cmd) {
    if (cmd.started_at !== null && cmd.finished_at !== null) {
      return dur(cmd.finished_at - cmd.started_at);
    }
    if (cmd.status === 'running' && cmd.started_at !== null) {
      return dur(Date.now() / 1000 - cmd.started_at);
    }
    return '';
  }

  /* The reply body, or null while the rule sits open waiting for it. */
  function replyBodyText(cmd) {
    switch (cmd.status) {
      case 'done':
        return (cmd.reply && cmd.reply.trim())
          ? cmd.reply
          : 'Finished with no reply body. Open the session in ' + DESTINATION + '.';
      case 'timed_out':
        return 'Lost track of this one. Open the session in ' + DESTINATION + '.';
      case 'failed':
        return (cmd.error && cmd.error.trim())
          ? cmd.error
          : 'Failed with no detail recorded. Open the session in ' + DESTINATION + '.';
      case 'cancelled':
        return 'Stopped in ' + DESTINATION + ' before it finished.';
      case 'held':
        return 'Held — injection is off.';
      default:
        return null; // queued, running
    }
  }

  /* Nothing was sent for a held request, so there is no [CMD]/[REPLY] pair to
     mirror. The second tag says so. */
  function replyKeyword(status) {
    return status === 'held' ? 'HELD' : 'REPLY';
  }

  /* ── model ──────────────────────────────────────────────────────────── */

  function normalize(raw) {
    if (!raw || typeof raw !== 'object') return null;
    var id = (raw.id === null || raw.id === undefined) ? '' : String(raw.id);
    if (!id) return null;
    return {
      id: id,
      text: typeof raw.text === 'string' ? raw.text : '',
      status: typeof raw.status === 'string' ? raw.status : 'queued',
      reply: typeof raw.reply === 'string' ? raw.reply : null,
      error: typeof raw.error === 'string' ? raw.error : null,
      created_at: num(raw.created_at),
      started_at: num(raw.started_at),
      finished_at: num(raw.finished_at),
      /* Built by internd from its own public base URL and stored verbatim, so
         this page never needs its own copy of that hostname. Absent on
         anything held or failed before it was ever accepted. */
      project_url: typeof raw.project_url === 'string' ? raw.project_url : null
    };
  }

  /* ── the tag: [KEYWORD-id], with the correlation id carrying the weight ─ */

  function span(cls, text) {
    var n = document.createElement('span');
    n.className = cls;
    n.textContent = text;
    return n;
  }

  function makeTag(keyword, id) {
    var node = document.createElement('span');
    node.className = 'tag';
    var kw = span('t-k', keyword);
    node.appendChild(span('t-b', '['));
    node.appendChild(kw);
    node.appendChild(span('t-b', '-'));
    node.appendChild(span('t-id', id));
    node.appendChild(span('t-b', ']'));
    return { node: node, kw: kw };
  }

  /* ── one entry: the tag pair, as it reads in the pane ────────────────── */

  function createRecord(cmd) {
    var root = document.createElement('article');
    root.className = 'entry';
    root.dataset.id = cmd.id;

    var cmdTag = makeTag('CMD', cmd.id);
    var cmdLine = document.createElement('div');
    cmdLine.className = 'tagline cmd-line';
    var time = document.createElement('time');
    time.className = 'meta time';
    cmdLine.appendChild(cmdTag.node);
    cmdLine.appendChild(span('rule', ''));
    cmdLine.appendChild(time);

    var cmdBody = document.createElement('p');
    cmdBody.className = 'body cmd-body';

    var replyTag = makeTag('REPLY', cmd.id);
    var replyLine = document.createElement('div');
    replyLine.className = 'tagline reply-line';
    var duration = document.createElement('span');
    duration.className = 'meta duration';
    var status = document.createElement('span');
    status.className = 'status';
    /* The way back to the full conversation. Hidden until internd has
       actually accepted the command and told us where it landed — a link to
       a session that does not exist yet is worse than no link. */
    var open = document.createElement('a');
    open.className = 'open-in';
    open.textContent = 'open';
    open.rel = 'noopener noreferrer';
    open.target = '_blank';
    open.hidden = true;
    replyLine.appendChild(replyTag.node);
    replyLine.appendChild(span('rule', ''));
    replyLine.appendChild(duration);
    replyLine.appendChild(status);
    replyLine.appendChild(open);

    var replyBody = document.createElement('div');
    replyBody.className = 'body reply-body';
    replyBody.hidden = true;

    root.appendChild(cmdLine);
    root.appendChild(cmdBody);
    root.appendChild(replyLine);
    root.appendChild(replyBody);

    return {
      cmd: cmd,
      root: root,
      time: time,
      cmdBody: cmdBody,
      replyKw: replyTag.kw,
      duration: duration,
      status: status,
      open: open,
      replyBody: replyBody,
      raf: 0,
      watchdog: 0,
      textPainted: false
    };
  }

  function apply(rec, cmd, animate) {
    var prev = rec.cmd;
    rec.cmd = cmd;

    rec.root.dataset.status = cmd.status;
    rec.time.textContent = clock(cmd.created_at);
    var iso = isoStamp(cmd.created_at);
    if (iso) rec.time.setAttribute('datetime', iso);

    if (!rec.textPainted) {
      rec.textPainted = true;
      paintText(rec, cmd.text, animate);
    } else if (prev && prev.text !== cmd.text && !rec.raf) {
      rec.cmdBody.textContent = cmd.text;
    }

    rec.replyKw.textContent = replyKeyword(cmd.status);
    rec.status.textContent = statusWord(cmd.status);
    rec.duration.textContent = durationText(cmd);

    /* Only ever an absolute https URL from internd. Assigning a string the
       daemon gave us to `href` would otherwise happily accept a
       `javascript:` scheme, and this page renders a log of text that came in
       over a webhook. */
    if (cmd.project_url && /^https?:\/\//i.test(cmd.project_url)) {
      rec.open.href = cmd.project_url;
      rec.open.setAttribute('aria-label', 'open command ' + cmd.id + ' in ' + DESTINATION);
      rec.open.hidden = false;
    } else {
      rec.open.removeAttribute('href');
      rec.open.hidden = true;
    }

    var body = replyBodyText(cmd);
    if (body === null) {
      rec.replyBody.hidden = true;
      rec.replyBody.textContent = '';
      rec.replyBody.removeAttribute('tabindex');
      rec.replyBody.removeAttribute('role');
      rec.replyBody.removeAttribute('aria-label');
    } else if (rec.replyBody.hidden || rec.replyBody.textContent !== body) {
      rec.replyBody.textContent = body;
      rec.replyBody.hidden = false;
      markScrollable(rec.replyBody);
    }
  }

  /* Long replies scroll in their own box; give that box a tab stop so it can
     be reached and scrolled from the keyboard. */
  function markScrollable(node) {
    if (node.scrollHeight > node.clientHeight + 2) {
      node.tabIndex = 0;
      node.setAttribute('role', 'region');
      node.setAttribute('aria-label', 'reply, scrollable');
    } else {
      node.removeAttribute('tabindex');
      node.removeAttribute('role');
      node.removeAttribute('aria-label');
    }
  }

  /* The one motion in the log: a newly arrived command types itself in with a
     caret, mirroring what is literally happening in the pane. Skipped when the
     reader is not looking (reduced motion, hidden tab), and backed by a
     watchdog so the text can never be left half-typed. */
  function paintText(rec, text, animate) {
    var node = rec.cmdBody;
    stopPaint(rec);

    if (!animate || reduceMotion || !text ||
        (document.visibilityState && document.visibilityState !== 'visible')) {
      node.textContent = text;
      return;
    }

    node.textContent = '';
    var typed = document.createElement('span');
    var caret = document.createElement('span');
    caret.className = 'caret';
    caret.setAttribute('aria-hidden', 'true');
    node.appendChild(typed);
    node.appendChild(caret);

    var total = Math.min(1100, Math.max(280, text.length * 18));
    var start = performance.now();

    function finish() {
      stopPaint(rec);
      node.textContent = text;
    }

    function frame(now) {
      var p = Math.min(1, (now - start) / total);
      typed.textContent = text.slice(0, Math.max(1, Math.ceil(p * text.length)));
      if (p < 1) {
        rec.raf = requestAnimationFrame(frame);
      } else {
        finish();
      }
    }

    rec.raf = requestAnimationFrame(frame);
    rec.watchdog = setTimeout(finish, total + 600);
  }

  function stopPaint(rec) {
    if (rec.raf) { cancelAnimationFrame(rec.raf); rec.raf = 0; }
    if (rec.watchdog) { clearTimeout(rec.watchdog); rec.watchdog = 0; }
  }

  /* ── list ───────────────────────────────────────────────────────────── */

  function sortKey(cmd) {
    return cmd.created_at === null ? 0 : cmd.created_at;
  }

  function insertIndex(cmd) {
    var key = sortKey(cmd);
    for (var i = 0; i < order.length; i++) {
      var other = records.get(order[i]).cmd;
      if (key > sortKey(other)) return i;
    }
    return order.length;
  }

  function upsert(raw, animate) {
    var cmd = normalize(raw);
    if (!cmd) return;

    var rec = records.get(cmd.id);
    if (rec) {
      var before = rec.cmd.status;
      apply(rec, cmd, false);
      if (before !== cmd.status && replyBodyText(cmd) !== null) {
        announce(cmd.id + ' ' + statusWord(cmd.status));
      }
    } else {
      rec = createRecord(cmd);
      var idx = insertIndex(cmd);
      var nextId = order[idx];
      records.set(cmd.id, rec);
      order.splice(idx, 0, cmd.id);
      el.entries.insertBefore(
        rec.root,
        nextId ? records.get(nextId).root : null
      );
      apply(rec, cmd, animate);
      if (animate) {
        announce(cmd.status === 'held'
          ? 'held, not typed: ' + cmd.id
          : 'new command ' + cmd.id);
      }
      trim();
    }

    updateEmpty();
    updateCount();
    syncRunning();
  }

  function trim() {
    while (order.length > MAX_ENTRIES) {
      var id = order.pop();
      var rec = records.get(id);
      if (rec) {
        stopPaint(rec);
        if (rec.root.parentNode) rec.root.parentNode.removeChild(rec.root);
      }
      records.delete(id);
    }
  }

  function updateEmpty() {
    el.empty.hidden = !(seeded && order.length === 0 && el.notice.hidden);
  }

  function updateCount() {
    if (!el.logCount) return;
    if (!order.length) { el.logCount.textContent = ''; return; }
    var held = 0;
    for (var i = 0; i < order.length; i++) {
      if (records.get(order[i]).cmd.status === 'held') held++;
    }
    var parts = [order.length + (order.length === 1 ? ' entry' : ' entries')];
    if (held) parts.push(held + ' held');
    el.logCount.textContent = parts.join(' · ');
  }

  function announce(msg) {
    if (el.announcer) el.announcer.textContent = msg;
  }

  /* ── running: the dot pulses only while a turn is live ───────────────── */

  function runningCount() {
    var n = 0;
    for (var i = 0; i < order.length; i++) {
      if (records.get(order[i]).cmd.status === 'running') n++;
    }
    return n;
  }

  function syncRunning() {
    var live = runningCount();

    if (live > 0 && !ticker) {
      ticker = setInterval(tickDurations, 100);
    } else if (live === 0 && ticker) {
      clearInterval(ticker);
      ticker = 0;
    }

    var state = !connected ? 'reconnecting' : (live > 0 ? 'running' : 'idle');
    el.dot.dataset.state = state;
    el.feedWord.textContent = state;
  }

  function tickDurations() {
    for (var i = 0; i < order.length; i++) {
      var rec = records.get(order[i]);
      if (rec.cmd.status === 'running') {
        rec.duration.textContent = durationText(rec.cmd);
      }
    }
  }

  /* ── notices ─────────────────────────────────────────────────────────── */

  function showNotice(msg) {
    el.notice.textContent = msg;
    el.notice.hidden = false;
    updateEmpty();
  }

  function clearNotice() {
    el.notice.hidden = true;
    el.notice.textContent = '';
    updateEmpty();
  }

  function showBreakerMsg(msg) {
    if (!el.breakerMsg) return;
    el.breakerMsg.textContent = msg;
    el.breakerMsg.hidden = false;
    announce(msg);
  }

  function clearBreakerMsg() {
    if (!el.breakerMsg) return;
    el.breakerMsg.hidden = true;
    el.breakerMsg.textContent = '';
  }

  /* ── the breaker ─────────────────────────────────────────────────────── */

  /* The tab strip is peripheral vision too: a filled vermilion square while
     the line is hot, a hollow outline while it is held. Painted into a canvas
     so it costs no request. */
  var FAVICON_INK = { live: '#FF3B1F', held: '#0B0D10', unknown: '#9CA3AF' };

  function paintFavicon(state) {
    if (!el.favicon) return;
    try {
      var c = document.createElement('canvas');
      c.width = 32; c.height = 32;
      var g = c.getContext && c.getContext('2d');
      if (!g) return;
      g.fillStyle = '#FFFFFF';
      g.fillRect(0, 0, 32, 32);
      var tone = FAVICON_INK[state] || FAVICON_INK.unknown;
      if (state === 'live') {
        g.fillStyle = tone;
        g.fillRect(5, 5, 22, 22);
      } else {
        g.strokeStyle = tone;
        g.lineWidth = 4;
        g.strokeRect(7, 7, 18, 18);
      }
      el.favicon.setAttribute('href', c.toDataURL());
    } catch (err) {
      /* the favicon is a nicety; never let it break the page */
    }
  }

  function setSub(on) {
    var node = el.breakerSub;
    if (!node) return;
    node.textContent = '';
    if (on === null) {
      node.textContent = 'Reading state from indexd.';
      return;
    }
    if (on) {
      node.appendChild(document.createTextNode('Speech becomes a session in '));
      node.appendChild(span('w', DESTINATION));
      node.appendChild(document.createTextNode('.'));
    } else {
      node.appendChild(document.createTextNode(
        'Requests still land and are logged. Nothing is sent.'));
    }
  }

  function applyInjection(value) {
    var known = (value === true || value === false);
    var changed = known && injecting !== value;
    injecting = known ? value : null;

    var state = !known ? 'unknown' : (value ? 'live' : 'held');
    // Four characters either way: the word swaps in place, and '----' is the
    // same "no reading" convention the clock uses, never a guess at the state.
    var word = !known ? '----' : (value ? 'LIVE' : 'HELD');

    document.documentElement.setAttribute('data-injection', state);

    if (el.breaker) {
      el.breaker.setAttribute('aria-checked', value === true ? 'true' : 'false');
      if (known) el.breaker.removeAttribute('aria-disabled');
      else el.breaker.setAttribute('aria-disabled', 'true');
    }
    if (el.breakerWord) el.breakerWord.textContent = word;
    if (el.chipWord) el.chipWord.textContent = word;
    setSub(known ? value : null);

    document.title = known ? word + ' · indexd' : 'indexd';
    if (el.themeColor) {
      el.themeColor.setAttribute('content', state === 'live' ? '#FF3B1F' : '#FFFFFF');
    }
    paintFavicon(state);

    if (changed) {
      announce(value
        ? 'injection live: speech becomes a session in ' + DESTINATION
        : 'injection held: nothing is sent');
    }
  }

  function toggleInjection() {
    if (injecting === null) return;           // never guess at the state
    var previous = injecting;
    var target = !previous;
    var seq = ++postSeq;

    clearBreakerMsg();
    applyInjection(target);                   // optimistic
    el.breaker.dataset.pending = '1';

    fetch('/api/injection', {
      method: 'POST',
      cache: 'no-store',
      credentials: 'same-origin',
      headers: {
        'content-type': 'application/json',
        'accept': 'application/json'
      },
      body: JSON.stringify({ enabled: target })
    }).then(function (res) {
      if (!res.ok) throw new Error('HTTP ' + res.status);
      return res.json();
    }).then(function (data) {
      if (seq !== postSeq) return;            // a newer click owns the state
      delete el.breaker.dataset.pending;
      applyInjection(
        (data && typeof data.injecting === 'boolean') ? data.injecting : target
      );
    }).catch(function (err) {
      if (seq !== postSeq) return;
      delete el.breaker.dataset.pending;
      applyInjection(previous);               // the server never agreed: revert
      showBreakerMsg(
        'Could not ' + (target ? 'go live' : 'hold') + ' — ' + err.message +
        '. Injection is still ' + (previous ? 'LIVE' : 'HELD') + '.'
      );
    });
  }

  if (el.breaker) {
    el.breaker.addEventListener('click', function (ev) {
      ev.preventDefault();
      if (el.breaker.getAttribute('aria-disabled') === 'true') return;
      toggleInjection();
    });
  }

  /* ── history ────────────────────────────────────────────────────────── */

  function load() {
    return fetch('/api/commands?limit=' + MAX_ENTRIES, {
      cache: 'no-store',
      credentials: 'same-origin',
      headers: { 'accept': 'application/json' }
    }).then(function (res) {
      if (!res.ok) throw new Error('HTTP ' + res.status);
      return res.json();
    }).then(function (data) {
      var list = (data && Array.isArray(data.commands)) ? data.commands : [];
      for (var i = list.length - 1; i >= 0; i--) {
        upsert(list[i], false);
      }
      seeded = true;
      clearNotice();
      updateEmpty();
      updateCount();
      syncRunning();
    }).catch(function (err) {
      showNotice(
        'Cannot read the command log from ' + (location.host || 'this host') +
        ' (' + err.message + '). Check that indexd is running — ' +
        'systemctl --user status indexd — then reload.'
      );
      if (!retryLoadTimer) {
        retryLoadTimer = setTimeout(function () {
          retryLoadTimer = 0;
          load();
        }, 5000);
      }
    });
  }

  /* Where commands go, and the breaker position, are the daemon's facts and
     not the page's. The markup carries sensible defaults so nothing is ever
     blank, but /api/info is the only thing that actually knows. */
  function loadInfo() {
    return fetch('/api/info', {
      cache: 'no-store',
      credentials: 'same-origin',
      headers: { 'accept': 'application/json' }
    }).then(function (r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.json();
    }).then(function (info) {
      if (!info) return;
      if (info.intern_url && el.internUrl) {
        el.internUrl.textContent = String(info.intern_url);
      }
      applyInjection(typeof info.injecting === 'boolean' ? info.injecting : null);
    }).catch(function () {
      // Leave the breaker reading "..." rather than asserting a state the
      // daemon never confirmed.
      applyInjection(null);
    });
  }

  /* ── live feed ──────────────────────────────────────────────────────── */

  function connect() {
    if (es) { es.close(); es = null; }

    var src;
    try {
      src = new EventSource('/api/events');
    } catch (err) {
      connected = false;
      syncRunning();
      scheduleReconnect();
      return;
    }
    es = src;

    src.onopen = function () {
      backoff = 1000;
      connected = true;
      syncRunning();
      if (openedOnce) { load(); loadInfo(); }  // resync what the gap hid
      openedOnce = true;
    };

    src.onmessage = function (ev) {
      var msg;
      try { msg = JSON.parse(ev.data); } catch (err) { return; }
      if (!msg || typeof msg !== 'object') return;

      if (msg.type === 'injection') {
        // The server is the authority: this wins over any optimistic flip,
        // and keeps every open tab agreeing about whether the line is hot.
        if (typeof msg.enabled === 'boolean') {
          if (el.breaker) delete el.breaker.dataset.pending;
          applyInjection(msg.enabled);
          clearBreakerMsg();
        }
        return;
      }

      if (msg.type !== 'command' || !msg.command) return;
      var id = (msg.command.id === null || msg.command.id === undefined)
        ? '' : String(msg.command.id);
      upsert(msg.command, seeded && !records.has(id));
    };

    src.onerror = function () {
      if (es !== src) return;
      src.close();
      es = null;
      connected = false;
      syncRunning();
      scheduleReconnect();
    };
  }

  function scheduleReconnect() {
    clearTimeout(reconnectTimer);
    var wait = Math.min(backoff, RECONNECT_MAX) * (0.8 + Math.random() * 0.4);
    reconnectTimer = setTimeout(connect, wait);
    backoff = Math.min(backoff * 2, RECONNECT_MAX);
  }

  /* ── boot ───────────────────────────────────────────────────────────── */

  el.host.textContent = location.hostname || 'index.example.com';
  el.port.textContent = location.port || '7490';

  document.addEventListener('visibilitychange', function () {
    if (document.visibilityState === 'visible' && !connected) {
      clearTimeout(reconnectTimer);
      backoff = 1000;
      connect();
    }
  });

  applyInjection(null);
  syncRunning();
  loadInfo();
  load();
  connect();
})();
