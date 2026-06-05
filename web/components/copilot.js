import { html, nothing } from 'lit';
import { ChatSession }   from '../lib/chat-session.js';
import { renderMsg }     from './copilot-render.js';

export class AppCopilot extends ChatSession {
  static properties = {
    _collapsed:     { state: true },
    _modelOpen:     { state: true },
    _recording:     { state: true },
    _hasTranscribe: { state: true },
  };

  constructor() {
    super();
    this._collapsed     = false;
    this._modelOpen     = false;
    this._hasTranscribe = false;
    this._recording     = false;
    this._resizing      = false;
    this._mediaRecorder = null;
    this._audioChunks   = [];
    this._onResizeMove  = this._onResizeMove.bind(this);
    this._onResizeUp    = this._onResizeUp.bind(this);
    this._onKeydown     = this._onKeydown.bind(this);
    this._onKeyup       = this._onKeyup.bind(this);
  }

  connectedCallback() {
    super.connectedCallback?.();
    this._checkTranscribe();
    window.addEventListener('keydown', this._onKeydown);
    window.addEventListener('keyup',   this._onKeyup);
  }

  disconnectedCallback() {
    super.disconnectedCallback?.();
    window.removeEventListener('keydown', this._onKeydown);
    window.removeEventListener('keyup',   this._onKeyup);
  }

  async _checkTranscribe() {
    try {
      const r = await fetch('/api/transcribe/has');
      this._hasTranscribe = r.status === 204;
    } catch {
      this._hasTranscribe = false;
    }
  }

  // ── DOM hooks ─────────────────────────────────────────────────────────────────

  _getInputContent() {
    return this.querySelector('.copilot-textarea')?.value.trim() ?? '';
  }

  _clearInput() {
    const t = this.querySelector('.copilot-textarea');
    if (!t) return;
    t.value = '';
    t.style.height = 'auto';
  }

  _autoResize(el) {
    el.style.height = 'auto';
    el.style.height = el.scrollHeight + 'px';
  }

  _scrollToBottom() {
    this.updateComplete.then(() => {
      const el = this.querySelector('.copilot-messages');
      if (el) el.scrollTop = el.scrollHeight;
    });
  }

  _onMessagePushed(item) {
    if (item.kind === 'pending_write') {
      this.updateComplete.then(() => {
        const panels = this.querySelectorAll('.copilot-approval');
        const el = panels[panels.length - 1];
        if (el) el.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
      });
    } else {
      this._scrollToBottom();
    }
  }

  // ── Resize ────────────────────────────────────────────────────────────────────

  _startResize(e) {
    this._resizing     = true;
    this._resizeStartX = e.clientX;
    this._resizeStartW = this.offsetWidth;
    window.addEventListener('mousemove', this._onResizeMove);
    window.addEventListener('mouseup',   this._onResizeUp);
    e.preventDefault();
  }

  _onResizeMove(e) {
    if (!this._resizing) return;
    const delta    = this._resizeStartX - e.clientX;
    const newWidth = Math.max(260, Math.min(720, this._resizeStartW + delta));
    document.documentElement.style.setProperty('--copilot-width', `${newWidth}px`);
  }

  _onResizeUp() {
    this._resizing = false;
    window.removeEventListener('mousemove', this._onResizeMove);
    window.removeEventListener('mouseup',   this._onResizeUp);
  }

  // ── Input ─────────────────────────────────────────────────────────────────────

  _handleKeydown(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      this._send();
    }
  }

  // ── CTRL+SPACE shortcut ───────────────────────────────────────────────────────

  _onKeydown(e) {
    if (!this._hasTranscribe) return;
    if (e.code === 'Space' && e.ctrlKey && !e.repeat) {
      e.preventDefault();
      if (!this._recording) this._startRecording(true);
    }
  }

  _onKeyup(e) {
    if (!this._hasTranscribe) return;
    if (e.code === 'Space' && this._recording && this._shortcutRecording) {
      e.preventDefault();
      this._stopRecording();
    }
  }

  // ── Recording ─────────────────────────────────────────────────────────────────

  async _startRecording(fromShortcut = false) {
    if (this._recording) return;
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      this._audioChunks      = [];
      this._shortcutRecording = fromShortcut;

      const mimeType = MediaRecorder.isTypeSupported('audio/webm;codecs=opus')
        ? 'audio/webm;codecs=opus'
        : MediaRecorder.isTypeSupported('audio/webm')
          ? 'audio/webm'
          : '';

      this._mediaRecorder = mimeType
        ? new MediaRecorder(stream, { mimeType })
        : new MediaRecorder(stream);

      this._mediaRecorder.addEventListener('dataavailable', e => {
        if (e.data.size > 0) this._audioChunks.push(e.data);
      });

      this._mediaRecorder.addEventListener('stop', () => {
        stream.getTracks().forEach(t => t.stop());
        this._submitAudio();
      });

      this._mediaRecorder.start();
      this._recording = true;
    } catch (err) {
      console.error('mic error:', err);
    }
  }

  _stopRecording() {
    if (!this._recording || !this._mediaRecorder) return;
    this._mediaRecorder.stop();
    this._recording = false;
  }

  _toggleRecording() {
    if (this._recording) {
      this._shortcutRecording = false;
      this._stopRecording();
    } else {
      this._startRecording(false);
    }
  }

  async _submitAudio() {
    if (this._audioChunks.length === 0) return;

    const mimeType = this._mediaRecorder?.mimeType ?? 'audio/webm';
    const blob = new Blob(this._audioChunks, { type: mimeType });
    // Derive file extension from mimeType, e.g. "audio/webm;codecs=opus" → "webm"
    const ext = mimeType.split('/')[1]?.split(';')[0] ?? 'webm';

    const form = new FormData();
    form.append('audio', blob, `recording.${ext}`);

    try {
      const resp = await fetch('/api/transcribe/audio', { method: 'POST', body: form });
      if (!resp.ok) throw new Error(await resp.text());
      const { text } = await resp.json();
      if (text) {
        const ta = this.querySelector('.copilot-textarea');
        if (ta) {
          ta.value = (ta.value ? ta.value + ' ' : '') + text;
          this._autoResize(ta);
          ta.focus();
        }
      }
    } catch (err) {
      console.error('transcription error:', err);
    }
  }

  // ── Render helpers ────────────────────────────────────────────────────────────

  _toggleExpand(id) {
    const next = new Set(this._expanded);
    if (next.has(id)) next.delete(id); else next.add(id);
    this._expanded = next;
  }

  // ── Render ────────────────────────────────────────────────────────────────────

  render() {
    if (this._collapsed) {
      return html`
        <div class="copilot-resize-handle" @mousedown=${(e) => this._startResize(e)}></div>
        <button
          class="copilot-expand-btn"
          title="Open copilot"
          @click=${() => { this._collapsed = false; }}
        >
          <i class="bi bi-stars"></i>
        </button>
      `;
    }

    return html`
      <div class="copilot-resize-handle" @mousedown=${(e) => this._startResize(e)}></div>

      <div class="copilot-header">
        <i class="bi bi-stars"></i>
        <span>Copilot</span>
        <button
          class="btn btn-sm btn-outline-secondary ms-auto copilot-collapse-btn"
          title="Collapse copilot"
          @click=${() => { this._collapsed = true; }}
        >
          <i class="bi bi-chevron-right"></i>
        </button>
      </div>

      <div class="copilot-messages">
        ${this._messages.length === 0 ? html`
          <div class="copilot-msg assistant">
            Hello! How can I help you today?
          </div>
        ` : this._messages.map(m => renderMsg(this, m))}

        ${this._waiting ? html`
          <div class="copilot-msg assistant copilot-thinking">
            <span class="spinner-border spinner-border-sm me-2" role="status"></span>
            Thinking…
          </div>
        ` : nothing}
      </div>

      <div class="copilot-input-area">
        <div class="copilot-composer">
          <textarea
            class="copilot-textarea"
            rows="1"
            placeholder="Ask the copilot… (Enter to send, Shift+Enter for new line)"
            @keydown=${this._handleKeydown}
            @input=${(e) => this._autoResize(e.target)}
            ?disabled=${this._waiting}
          ></textarea>
          <div class="copilot-toolbar">
            <div class="copilot-toolbar-left">
              ${this._providers.length > 1 ? html`
                <div class="copilot-model-wrap">
                  ${this._modelOpen ? html`
                    <div class="copilot-model-overlay" @click=${() => { this._modelOpen = false; }}></div>
                    <div class="copilot-model-dropdown">
                      ${this._providers.map(p => html`
                        <button
                          class="copilot-model-item ${p === this._selectedClient ? 'active' : ''}"
                          @click=${() => { this._selectedClient = p; this._modelOpen = false; }}
                        >${p}</button>
                      `)}
                    </div>
                  ` : nothing}
                  <button class="copilot-model-pill" @click=${() => { this._modelOpen = !this._modelOpen; }}>
                    <i class="bi bi-stars"></i>
                    <span>${this._selectedClient ?? 'auto'}</span>
                    <i class="bi bi-chevron-${this._modelOpen ? 'down' : 'up'}"></i>
                  </button>
                </div>
              ` : nothing}
              <button
                class="copilot-toolbar-btn"
                title="New session"
                @click=${() => this._startNewSession()}
              ><i class="bi bi-trash"></i></button>
            </div>
            <div class="copilot-toolbar-right">
              ${this._hasTranscribe ? html`
                <button
                  class="copilot-send-btn ${this._recording ? 'copilot-send-btn--recording' : ''}"
                  title="${this._recording ? 'Stop recording' : 'Record voice (Ctrl+Space)'}"
                  @click=${() => this._toggleRecording()}
                >
                  <i class="bi ${this._recording ? 'bi-stop-circle-fill' : 'bi-mic-fill'}"></i>
                </button>
              ` : nothing}
              ${this._waiting
                ? html`<button class="copilot-send-btn copilot-send-btn--stop" @click=${() => this._cancel()} title="Stop">
                    <i class="bi bi-stop-fill"></i>
                  </button>`
                : html`<button class="copilot-send-btn" @click=${() => this._send()}>
                    <i class="bi bi-send-fill"></i>
                  </button>`
              }
            </div>
          </div>
        </div>
      </div>
    `;
  }
}
