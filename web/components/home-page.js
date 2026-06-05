import { html, nothing } from 'lit';
import { LightElement }  from '../lib/base.js';
import { InboxMixin }    from '../lib/inbox-mixin.js';

const GUIDE = [
  {
    icon:  'bi-chat-dots-fill',
    title: 'Copilot',
    desc:  'The chat panel on the right knows everything — ask it to run agents, enable plugins, write code, or search the web.',
    color: '#0d6efd',
  },
  {
    icon:  'bi-inbox',
    title: 'Inbox',
    desc:  'Pending approvals and agent questions that need your input before background tasks can continue.',
    color: '#f59e0b',
  },
  {
    icon:  'bi-people',
    title: 'Agents',
    desc:  'Specialized sub-agents (engineer, architect, QA…). Each has a focused system prompt, tool set, and model selection.',
    color: '#8b5cf6',
  },
  {
    icon:  'bi-clock',
    title: 'Cron',
    desc:  'Scheduled tasks that run automatically at set intervals, even when the Copilot is idle.',
    color: '#f97316',
  },
  {
    icon:  'bi-cpu',
    title: 'Models',
    desc:  'Manage LLM, transcription, and image generation models. Drag to reorder priority.',
    color: '#10b981',
  },
  {
    icon:  'bi-plug',
    title: 'Providers',
    desc:  'Add API keys for LLM providers (Anthropic, OpenAI, OpenRouter, Ollama…).',
    color: '#06b6d4',
  },
  {
    icon:  'bi-shield-check',
    title: 'Approval Rules',
    desc:  'Define rules to auto-approve or auto-reject tool calls — skip repetitive confirmation prompts.',
    color: '#ef4444',
  },
];

export class HomePage extends InboxMixin(LightElement) {

  static get properties() {
    return {
      ...super.properties,
      _open:         { state: true },
      _models:       { state: true },
      _plugins:      { state: true },
      _debugMode:    { state: true },
      _debugLoading: { state: true },
    };
  }

  constructor() {
    super();
    this._open         = false;
    this._models       = null;   // null = loading, [] = no models configured
    this._plugins      = null;
    this._pollTimer    = null;
    this._debugMode    = false;
    this._debugLoading = true;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'home';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) {
        this._loadAll();
        this._startPolling();
      } else {
        this._stopPolling();
      }
    });
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this._stopPolling();
  }

  _startPolling() {
    this._stopPolling();
    this._pollTimer = setInterval(() => this._loadAll(), 10000);
  }

  _stopPolling() {
    if (this._pollTimer) {
      clearInterval(this._pollTimer);
      this._pollTimer = null;
    }
  }

  async _loadAll() {
    await Promise.all([
      this._loadModels(),
      this._loadPlugins(),
      this._loadInbox(),
      this._loadDebugMode(),
    ]);
  }

  async _loadDebugMode() {
    try {
      const res = await fetch('/api/dev/debug_mode');
      if (!res.ok) throw new Error();
      const data = await res.json();
      this._debugMode = data.enabled;
    } catch {
      // ignore, keep current value
    } finally {
      this._debugLoading = false;
    }
  }

  async _toggleDebugMode() {
    const next = !this._debugMode;
    this._debugMode = next;
    try {
      const res = await fetch('/api/dev/debug_mode', {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ enabled: next }),
      });
      if (!res.ok) throw new Error();
      window.dispatchEvent(new CustomEvent('debug-mode-change', { detail: { enabled: next } }));
    } catch {
      this._debugMode = !next;
    }
  }

  async _loadModels() {
    try {
      const res = await fetch('/api/llm/models');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._models = await res.json();
    } catch {
      this._models = [];
    }
  }

  async _loadPlugins() {
    try {
      const res = await fetch('/api/plugins');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._plugins = await res.json();
    } catch {
      this._plugins = [];
    }
  }

  get _honchoActive() {
    return this._plugins?.some(p => p.id === 'honcho' && p.enabled && p.running) ?? false;
  }

  get _statusInfo() {
    if (this._models === null)       return { cls: 'loading', dot: false, icon: null,                    text: 'Loading…' };
    if (this._models.length === 0)   return { cls: 'error',   dot: false, icon: 'bi-exclamation-circle-fill', text: 'No LLM models' };
    if (this._models.some(m => m.status === 'healthy'))  return { cls: 'online',  dot: true,  icon: null,                    text: 'Online & ready' };
    if (this._models.some(m => m.status === 'degraded')) return { cls: 'warn',    dot: true,  icon: 'bi-exclamation-triangle-fill', text: 'Degraded' };
    return { cls: 'error', dot: false, icon: 'bi-exclamation-circle-fill', text: 'All models offline' };
  }

  _nav(page) {
    const url = page === 'home' ? location.pathname : '#' + page;
    history.pushState({ page }, '', url);
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page } }));
  }

  render() {
    const st         = this._statusInfo;
    const noModels   = this._models !== null && this._models.length === 0;
    const approvals  = this._inboxData?.approvals      ?? [];
    const clarifs    = this._inboxData?.clarifications ?? [];
    const inboxTotal = approvals.length + clarifs.length;

    return html`
      <div class="home-page">

        <!-- ── Debug toggle ── -->
        <div class="home-debug-bar">
          <label class="home-debug-toggle" title="${this._debugMode ? 'Debug mode on' : 'Debug mode off'}">
            <i class="bi bi-bug-fill"></i>
            <span>Debug</span>
            <div class="form-check form-switch mb-0">
              <input class="form-check-input" type="checkbox"
                     .checked=${this._debugMode}
                     @change=${this._toggleDebugMode}
                     ?disabled=${this._debugLoading} />
            </div>
          </label>
        </div>

        <!-- ── Hero ── -->
        <div class="home-hero">
          <div class="home-hero-image">
            <img src="/assets/icons/icon-1024.png" alt="Skald" />
          </div>
          <div class="home-hero-text">
            <h1 class="home-hero-title">Skald</h1>
            <p class="home-hero-desc">Your AI command centre — research, code, plan, and orchestrate. All in one place.</p>
            <div class="home-hero-status home-hero-status--${st.cls}">
              ${st.dot  ? html`<span class="home-hero-dot"></span>` : nothing}
              ${st.icon ? html`<i class="bi ${st.icon}"></i>` : nothing}
              <span>${st.text}</span>
            </div>
          </div>
        </div>

        <!-- ── No-models banner ── -->
        ${noModels ? html`
          <div class="home-banner home-banner--error">
            <div class="home-banner-icon"><i class="bi bi-cpu-fill"></i></div>
            <div class="home-banner-body">
              <strong>No LLM models configured.</strong>
              Start by adding a provider (Anthropic, OpenAI, OpenRouter…), then add at least one model in the Models section.
            </div>
            <button class="btn btn-sm btn-danger" @click=${() => this._nav('providers')}>
              Add a provider
            </button>
          </div>
        ` : nothing}

        <!-- ── Pending inbox ── -->
        <div class="home-section-title">
          <i class="bi bi-inbox"></i>
          <span>Pending</span>
          ${inboxTotal > 0 ? html`<span class="badge bg-danger">${inboxTotal}</span>` : nothing}
          <button class="inbox-refresh-btn ms-auto" title="Refresh" @click=${() => this._loadInbox()}>
            <i class="bi bi-arrow-clockwise"></i>
          </button>
        </div>
        ${this._renderInboxSection()}

        <!-- ── Honcho tip ── -->
        ${!this._honchoActive ? html`
          <div class="home-tip">
            <div class="home-tip-icon"><i class="bi bi-lightbulb-fill"></i></div>
            <div class="home-tip-body">
              <strong>Enable Honcho</strong>
              <span>Persistent long-term memory — the agent learns your preferences over time. Ask the Copilot to enable it.</span>
            </div>
          </div>
        ` : nothing}

        <!-- ── Quick guide ── -->
        <div class="home-section-title">
          <i class="bi bi-map"></i>
          <span>Quick guide</span>
        </div>
        <div class="home-guide">
          ${GUIDE.map(s => html`
            <div class="home-card" style="--home-card-color: ${s.color}">
              <div class="home-card-icon">
                <i class="bi ${s.icon}"></i>
              </div>
              <div class="home-card-body">
                <h6 class="home-card-title">${s.title}</h6>
                <p class="home-card-desc">${s.desc}</p>
              </div>
            </div>
          `)}
        </div>
      </div>
    `;
  }
}
