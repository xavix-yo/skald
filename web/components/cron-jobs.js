import { html } from 'lit';
import { LightElement } from '../lib/base.js';
import { toString as cronToString } from 'cronstrue';

function formatDate(iso) {
  if (!iso) return '—';
  return new Date(iso).toLocaleString('en-GB', { day: '2-digit', month: '2-digit', year: '2-digit', hour: '2-digit', minute: '2-digit' });
}

export class TasksPage extends LightElement {
  static properties = {
    _jobs:    { state: true },
    _error:   { state: true },
    _open:    { state: true },
  };

  constructor() {
    super();
    this._jobs  = [];
    this._error = null;
    this._open  = false;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'tasks';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
  }

  async _load() {
    try {
      const res = await fetch('/api/cron/jobs');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._jobs = await res.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  async _delete(job) {
    if (!confirm(`Delete job "${job.title}"?`)) return;
    try {
      const res = await fetch(`/api/cron/jobs/${job.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  async _toggle(job) {
    try {
      const res = await fetch(`/api/cron/jobs/${job.id}/toggle`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ enabled: !job.enabled }),
      });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  _renderCard(job) {
    const isImmediate = job.kind === 'immediate';
    return html`
      <div class="task-card ${job.enabled ? '' : 'task-card--disabled'}">
        <div class="task-card-header">
          <div class="task-card-title-row">
            <span class="task-card-title">${job.title}</span>
            ${isImmediate ? html`<span class="task-badge task-badge--immediate">Immediate</span>` : ''}
            ${!isImmediate && job.single_run ? html`<span class="task-badge task-badge--oneshot">one-shot</span>` : ''}
          </div>
          <button class="task-card-delete" title="Delete" @click=${() => this._delete(job)}>
            <i class="bi bi-trash"></i>
          </button>
        </div>

        ${job.description ? html`<div class="task-card-desc">${job.description}</div>` : ''}

        ${!isImmediate ? html`
        <div class="task-card-expr">
          <i class="bi bi-clock"></i>
          <div class="task-card-expr-text">
            <span class="task-card-human">${cronToString(job.cron)}</span>
            <code class="task-card-raw">${job.cron}</code>
          </div>
        </div>
        ` : ''}

        <div class="task-card-meta">
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">Agent</span>
            <span class="task-card-meta-value">${job.agent_id}</span>
          </div>
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">Last run</span>
            <span class="task-card-meta-value">${formatDate(job.last_run_at)}</span>
          </div>
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">${isImmediate ? 'Created' : 'Next run'}</span>
            <span class="task-card-meta-value">${isImmediate ? formatDate(job.created_at) : formatDate(job.next_run_at)}</span>
          </div>
        </div>

        <div class="task-card-footer">
          ${!isImmediate ? html`
          <div class="form-check form-switch mb-0 task-card-toggle">
            <input class="form-check-input" type="checkbox" role="switch"
              .checked=${job.enabled}
              @change=${() => this._toggle(job)} />
            <span class="task-card-toggle-label">${job.enabled ? 'Enabled' : 'Disabled'}</span>
          </div>
          ` : ''}
        </div>
      </div>
    `;
  }

  render() {
    return html`
      <div class="task-page">
        <div class="task-page-header">
          <h2 class="task-page-title"><i class="bi bi-lightning-charge"></i> Tasks</h2>
          <div style="font-size:0.82rem;color:var(--bs-secondary-color)">
            ${this._jobs.length} task${this._jobs.length !== 1 ? 's' : ''}
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : ''}

        ${this._jobs.length === 0 ? html`
          <div class="task-empty">
            <i class="bi bi-lightning-charge"></i>
            <p>No tasks configured. Ask the agent to create one with <code>add_cron_job</code>.</p>
          </div>
        ` : html`
          <div class="task-grid">
            ${this._jobs.map(j => this._renderCard(j))}
          </div>
        `}

        <div class="task-footer-note">
          <i class="bi bi-info-circle"></i>
          Completed one-shot jobs are automatically deleted after a configurable number of days.
        </div>
      </div>
    `;
  }
}
