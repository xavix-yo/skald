import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';


export class AppSidebar extends LightElement {
  static properties = {
    _activePage:  { state: true },
    _inboxCount:  { state: true },
    _debugMode:   { state: true },
  };

  constructor() {
    super();
    this._activePage = null;
    this._inboxCount = 0;
    this._pollTimer  = null;
    this._debugMode  = false;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('popstate', (e) => {
      this._applyPage(e.state?.page ?? this._pageFromHash());
    });
    window.addEventListener('hashchange', () => {
      this._applyPage(this._pageFromHash());
    });
    window.addEventListener('inbox-count', (e) => {
      this._inboxCount = e.detail.count;
    });
    window.addEventListener('debug-mode-change', (e) => {
      this._debugMode = e.detail.enabled;
    });
    // On load: home (root) if no hash, otherwise the matching page
    setTimeout(() => this._applyPage(this._pageFromHash()), 0);
    // Poll inbox count independently of whether the page is open.
    this._pollInbox();
    this._pollTimer = setInterval(() => this._pollInbox(), 10000);
    this._loadDebugMode();
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    clearInterval(this._pollTimer);
  }

  async _loadDebugMode() {
    try {
      const res = await fetch('/api/dev/debug_mode');
      if (res.ok) this._debugMode = (await res.json()).enabled;
    } catch { /* ignore */ }
  }

  async _pollInbox() {
    try {
      const res = await fetch('/api/inbox');
      if (res.ok) {
        const data = await res.json();
        this._inboxCount = data.total ?? 0;
      }
    } catch { /* ignore */ }
  }

  _pageFromHash() {
    const hash = location.hash.slice(1);
    if (!hash) return 'home';
    const segment = hash.split('/')[0];
    return ['inbox', 'tasks', 'models', 'providers', 'approval', 'agents', 'llm-requests'].includes(segment) ? segment : 'home';
  }

  _applyPage(page) {
    this._activePage = page;
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page } }));
  }

  _togglePage(page, e) {
    e.preventDefault();
    if (page === 'home') {
      history.pushState({ page: 'home' }, '', location.pathname + location.search);
      this._applyPage('home');
      return;
    }
    if (this._activePage === page) {
      // In a sub-section (e.g. #models/image) → go back to page root
      if (location.hash.slice(1) !== page) {
        history.pushState({ page }, '', '#' + page);
        this._applyPage(page);
      }
      return;
    }
    history.pushState({ page }, '', '#' + page);
    this._applyPage(page);
  }

  render() {
    return html`
      <div class="sidebar-brand">
        <img src="/assets/icons/icon-1024.png" alt="" class="sidebar-brand-icon" />
        <span>Skald</span>
      </div>

      <hr class="sidebar-divider" />

      <nav class="sidebar-nav">
        <a href="#" class="sidebar-link ${this._activePage === 'home' ? 'active' : ''}"
           @click=${(e) => this._togglePage('home', e)}>
          <i class="bi bi-house-door"></i>
          <span class="sidebar-link-name">Home</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'inbox' ? 'active' : ''}"
           @click=${(e) => this._togglePage('inbox', e)}>
          <i class="bi bi-inbox"></i>
          <span class="sidebar-link-name">
            Agent Inbox
            ${this._inboxCount > 0
              ? html`<span class="badge bg-danger ms-1" style="font-size:0.65rem">${this._inboxCount}</span>`
              : ''}
          </span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'tasks' ? 'active' : ''}"
           @click=${(e) => this._togglePage('tasks', e)}>
          <i class="bi bi-lightning-charge"></i>
          <span class="sidebar-link-name">Tasks</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'models' ? 'active' : ''}"
           @click=${(e) => this._togglePage('models', e)}>
          <i class="bi bi-cpu"></i>
          <span class="sidebar-link-name">Models</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'providers' ? 'active' : ''}"
           @click=${(e) => this._togglePage('providers', e)}>
          <i class="bi bi-plug"></i>
          <span class="sidebar-link-name">Providers</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'approval' ? 'active' : ''}"
           @click=${(e) => this._togglePage('approval', e)}>
          <i class="bi bi-shield-check"></i>
          <span class="sidebar-link-name">Approval Rules</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'agents' ? 'active' : ''}"
           @click=${(e) => this._togglePage('agents', e)}>
          <i class="bi bi-people"></i>
          <span class="sidebar-link-name">Agents</span>
        </a>

        ${this._debugMode ? html`
          <hr class="sidebar-divider" />
          <a href="#llm-requests"
             class="sidebar-link ${this._activePage === 'llm-requests' ? 'active' : ''}"
             @click=${(e) => this._togglePage('llm-requests', e)}>
            <i class="bi bi-journal-code"></i>
            <span class="sidebar-link-name">LLM Requests</span>
          </a>
        ` : nothing}
      </nav>

    `;
  }
}
