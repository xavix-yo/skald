import { html, nothing } from 'lit';
import { ChatSession }   from '../../lib/chat-session.js';
import { renderMsg }     from '../copilot-render.js';

export class ChatPage extends ChatSession {
  static properties = {
    visible: { type: Boolean },
    // Target source. Defaults to the main mobile session; set to `project-{id}`
    // to bind this chat to a project's coordinator session.
    source:  { type: String },
    // Human-readable label for the active source (e.g. the project name), shown
    // in the header when inside a project.
    label:   { type: String },
  };

  constructor() {
    super();
    this.visible = false;
    this.source  = 'mobile';
    this.label   = '';
  }

  connectedCallback() {
    // Honour the initial `source` prop on the first connect so a cold deep-link
    // (e.g. the native shell opening #chat/project-<id>) connects straight to it,
    // instead of connecting to the 'mobile' default and switching a tick later
    // (which would briefly open two WebSockets). Later `source` prop changes are
    // still handled by `updated` below.
    if (this.source && this.source !== this._wsSource) this._activeSource = this.source;
    super.connectedCallback();
  }

  updated(changed) {
    if (changed.has('visible') && this.visible) {
      this._scrollToBottom();
    }
    // The owner (mobile-app) re-points this chat by changing `source`. Switch the
    // live connection — base `_switchSource` tears down the WS, reloads that
    // source's history, and reconnects. The guard skips the initial no-op render.
    if (changed.has('source') && this.source !== this._source) {
      this._switchSource(this.source);
    }
  }

  // ── Source identity ────────────────────────────────────────────────────────

  // Static fallback used only before the first `source` prop is applied.
  get _wsSource() { return 'mobile'; }

  get _inProject() {
    return typeof this.source === 'string' && this.source.startsWith('project-');
  }

  _exitProject() {
    this.dispatchEvent(new CustomEvent('project-exit', { bubbles: true, composed: true }));
  }

  // ── DOM hooks ──────────────────────────────────────────────────────────────

  _getInputContent() {
    return this.querySelector('.chat-page-textarea')?.value.trim() ?? '';
  }

  _clearInput() {
    const t = this.querySelector('.chat-page-textarea');
    if (t) t.value = '';
  }

  _scrollToBottom() {
    this.updateComplete.then(() => {
      const el = this.querySelector('.chat-page-messages');
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

  // ── Input ──────────────────────────────────────────────────────────────────

  _handleKeydown(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      this._send();
    }
  }

  // ── Toggle expand ──────────────────────────────────────────────────────────

  _toggleExpand(id) {
    const next = new Set(this._expanded);
    if (next.has(id)) next.delete(id); else next.add(id);
    this._expanded = next;
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this.visible) return nothing;

    return html`
      <div class="chat-page">

        <div class="mobile-section-header">
          <span class="mobile-section-title">
            ${this._inProject ? html`
              <button class="chat-page-back" title="Back to General"
                      @click=${() => this._exitProject()}>
                <i class="bi bi-chevron-left"></i>
              </button>
              <i class="bi bi-folder2-open"></i> ${this.label || 'Project'}
            ` : html`<i class="bi bi-chat-dots-fill"></i> Chat`}
          </span>
          <div class="chat-page-header-actions">
            ${this._providers.length > 1 ? html`
              <select
                class="form-select form-select-sm chat-page-provider-select"
                .value=${this._selectedClient ?? ''}
                @change=${(e) => { this._selectClient(e.target.value); }}
              >
                ${this._providers.map(p => html`
                  <option value=${p} ?selected=${p === this._selectedClient}>${p}</option>
                `)}
              </select>
            ` : nothing}
            <button
              class="btn btn-sm btn-outline-secondary"
              title="New conversation"
              @click=${() => this._startNewSession()}
            ><i class="bi bi-trash"></i></button>
          </div>
        </div>

        <div class="chat-page-messages">
          ${this._messages.length === 0 ? html`
            <div class="chat-page-empty">
              <i class="bi bi-stars"></i>
              <p>Ask me anything</p>
            </div>
          ` : this._messages.map(m => renderMsg(this, m))}

          ${this._waiting ? html`
            <div class="copilot-msg assistant copilot-thinking">
              <span class="spinner-border spinner-border-sm me-2" role="status"></span>
              Thinking…
            </div>
          ` : nothing}
        </div>

        <div class="chat-page-input-area">
          <div class="chat-page-input-row">
            <textarea
              class="form-control chat-page-textarea"
              rows="2"
              placeholder="Message… (Enter to send)"
              @keydown=${this._handleKeydown}
              ?disabled=${this._waiting}
            ></textarea>
            <div class="chat-page-input-actions">
              ${this._waiting
                ? html`<button
                    class="btn btn-danger chat-page-send"
                    @click=${() => this._cancel()}
                    title="Stop"
                  ><i class="bi bi-stop-fill"></i></button>`
                : html`<button
                    class="btn btn-primary chat-page-send"
                    @click=${() => this._send()}
                  ><i class="bi bi-send-fill"></i></button>`
              }
            </div>
          </div>
        </div>

      </div>
    `;
  }
}

customElements.define('chat-page', ChatPage);
