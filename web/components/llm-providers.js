import { html } from 'lit';
import { LightElement } from '../lib/base.js';

const PROVIDER_TYPE_LABELS = {
  anthropic:   'Anthropic',
  open_ai:     'OpenAI',
  openrouter:  'OpenRouter',
  deepseek:    'DeepSeek',
  ollama:      'Ollama',
  lm_studio:   'LM Studio',
  elevenlabs:  'ElevenLabs',
};

const PROVIDER_TYPES = Object.keys(PROVIDER_TYPE_LABELS);

const TYPE_COLORS = {
  anthropic:   '#d4a574',
  open_ai:     '#10a37f',
  openrouter:  '#8b5cf6',
  deepseek:    '#0ea5e9',
  ollama:      '#f97316',
  lm_studio:   '#6b7280',
  elevenlabs:  '#f59e0b',
};

const TYPE_ICONS = {
  anthropic:   'bi-chat-square-dots',
  open_ai:     'bi-lightning-charge',
  openrouter:  'bi-hdd-stack',
  deepseek:    'bi-search',
  ollama:      'bi-terminal',
  lm_studio:   'bi-window-stack',
  elevenlabs:  'bi-waveform',
};

function emptyForm() {
  return { name: '', type: 'anthropic', api_key: '', base_url: '', description: '' };
}

export class LlmProvidersPage extends LightElement {
  static properties = {
    _open:      { state: true },
    _providers: { state: true },
    _modelCounts: { state: true },
    _modal:     { state: true },
    _saving:    { state: true },
    _error:     { state: true },
    _form:      { state: true },
  };

  constructor() {
    super();
    this._open      = false;
    this._providers = [];
    this._modelCounts = {};
    this._modal     = null;
    this._saving    = false;
    this._error     = null;
    this._form      = emptyForm();
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'providers';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
  }

  async _load() {
    try {
      const [provRes, modelsRes] = await Promise.all([
        fetch('/api/llm/providers'),
        fetch('/api/llm/models'),
      ]);
      if (!provRes.ok)  throw new Error(`Providers: HTTP ${provRes.status}`);
      if (!modelsRes.ok) throw new Error(`Models: HTTP ${modelsRes.status}`);
      const providers = await provRes.json();
      const models    = await modelsRes.json();

      // Count models per provider
      const counts = {};
      for (const m of models) {
        const pid = String(m.provider_id);
        counts[pid] = (counts[pid] || 0) + 1;
      }

      this._providers   = providers;
      this._modelCounts = counts;
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── CRUD ──────────────────────────────────────────────────────────────────

  _openAdd() {
    this._error = null;
    this._form  = emptyForm();
    this._modal = { mode: 'add' };
  }

  async _openEdit(provider) {
    this._error = null;
    try {
      const res = await fetch(`/api/llm/providers/${provider.id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const record = await res.json();
      this._form = {
        name:        record.name,
        type:        record.type,
        api_key:     record.api_key  ?? '',
        base_url:    record.base_url ?? '',
        description: record.description ?? '',
      };
      this._modal = { mode: 'edit', id: record.id };
    } catch (e) {
      this._error = e.message;
    }
  }

  async _delete(provider) {
    if (!confirm(`Delete provider "${provider.name}"? All associated models will be deleted too.`)) return;
    try {
      const res = await fetch(`/api/llm/providers/${provider.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  async _onSubmit(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;

    const f = this._form;
    const needsBaseUrl = f.type === 'ollama' || f.type === 'lm_studio';
    const payload = {
      name:        f.name,
      type:        f.type,
      api_key:     f.api_key     || null,
      base_url:    needsBaseUrl ? (f.base_url || null) : null,
      description: f.description || null,
    };

    const isEdit = this._modal?.mode === 'edit';
    const url    = isEdit ? `/api/llm/providers/${this._modal.id}` : '/api/llm/providers';

    try {
      const res = await fetch(url, {
        method:  isEdit ? 'PUT' : 'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify(payload),
      });
      if (!res.ok) throw new Error(await res.text());
      this._modal = null;
      await this._load();
    } catch (err) {
      this._error = err.message;
    } finally {
      this._saving = false;
    }
  }

  _setField(field, value) {
    this._form = { ...this._form, [field]: value };
  }

  _closeModal() { this._modal = null; this._error = null; }

  // ── Render helpers ────────────────────────────────────────────────────────

  _renderCard(p) {
    const color    = TYPE_COLORS[p.type] ?? '#888';
    const icon     = TYPE_ICONS[p.type] ?? 'bi-box';
    const label    = PROVIDER_TYPE_LABELS[p.type] ?? p.type;
    const count    = this._modelCounts[String(p.id)];
    const hasKey   = Boolean(p.api_key);
    const needsUrl = p.type === 'ollama' || p.type === 'lm_studio';

    return html`
      <div class="pv-card" style="--pv-color: ${color}">
        <div class="pv-card-row1">
          <div class="pv-card-icon" style="background: color-mix(in srgb, ${color} 14%, transparent); color: ${color}">
            <i class="bi ${icon}"></i>
          </div>
          <span class="pv-card-name">${p.name}</span>
          <span class="pv-card-type-badge">${label}</span>
          ${count != null ? html`
            <span class="pv-card-count" title="Models using this provider">
              <i class="bi bi-cpu me-1"></i>${count}
            </span>
          ` : ''}
          <div class="pv-card-actions">
            <button class="pv-btn-icon pv-btn-edit" title="Edit" @click=${() => this._openEdit(p)}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="pv-btn-icon pv-btn-delete" title="Delete" @click=${() => this._delete(p)}>
              <i class="bi bi-trash"></i>
            </button>
          </div>
        </div>

        ${p.description ? html`
          <div class="pv-card-row2">
            <span class="pv-card-desc">${p.description}</span>
          </div>
        ` : ''}

        <div class="pv-card-row3">
          <span class="pv-card-tag ${hasKey ? 'pv-tag-ok' : 'pv-tag-missing'}">
            <i class="bi ${hasKey ? 'bi-lock-fill' : 'bi-unlock'}"></i>
            API key ${hasKey ? 'configured' : 'missing'}
          </span>
          ${needsUrl && p.base_url ? html`
            <span class="pv-card-tag pv-tag-url" title="Base URL">
              <i class="bi bi-link-45deg"></i>
              <span class="pv-card-url-text">${p.base_url}</span>
            </span>
          ` : ''}
          ${p.created_at ? html`
            <span class="pv-card-tag pv-tag-date">
              <i class="bi bi-calendar3"></i>
              ${new Date(p.created_at).toLocaleDateString()}
            </span>
          ` : ''}
        </div>
      </div>
    `;
  }

  // ── Modal ─────────────────────────────────────────────────────────────────

  _renderModal() {
    const isEdit     = this._modal?.mode === 'edit';
    const f          = this._form;
    const needsKey   = f.type !== 'ollama' && f.type !== 'lm_studio';
    const needsUrl   = f.type === 'ollama' || f.type === 'lm_studio';

    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog pv-modal">
          <div class="pv-modal-header">
            <i class="bi bi-plug"></i>
            <span>${isEdit ? 'Edit Provider' : 'Add Provider'}</span>
            <button type="button" class="pv-modal-close" @click=${() => this._closeModal()}>
              <i class="bi bi-x"></i>
            </button>
          </div>

          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}

          <form @submit=${(e) => this._onSubmit(e)}>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Name</label>
              <input type="text" class="form-control form-control-sm" .value=${f.name} required
                placeholder="e.g. My Anthropic" @input=${(e) => this._setField('name', e.target.value)} />
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Type</label>
              <select class="form-select form-select-sm" .value=${f.type}
                @change=${(e) => this._setField('type', e.target.value)}>
                ${PROVIDER_TYPES.map(t => html`<option value=${t}>${PROVIDER_TYPE_LABELS[t]}</option>`)}
              </select>
            </div>

            ${needsKey ? html`
              <div class="mb-3">
                <label class="form-label fw-semibold" style="font-size:0.82rem">API Key</label>
                <input type="password" class="form-control form-control-sm" .value=${f.api_key}
                  autocomplete="new-password"
                  placeholder=${isEdit ? 'Leave blank to keep existing key' : ''}
                  @input=${(e) => this._setField('api_key', e.target.value)} />
              </div>
            ` : ''}

            ${needsUrl ? html`
              <div class="mb-3">
                <label class="form-label fw-semibold" style="font-size:0.82rem">Base URL</label>
                <input type="text" class="form-control form-control-sm" .value=${f.base_url}
                  placeholder=${f.type === 'ollama' ? 'http://localhost:11434' : 'http://localhost:1234/v1'}
                  @input=${(e) => this._setField('base_url', e.target.value)} />
              </div>
            ` : ''}

            <div class="mb-4">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Description <span class="text-muted fw-normal">(optional)</span></label>
              <input type="text" class="form-control form-control-sm" .value=${f.description}
                @input=${(e) => this._setField('description', e.target.value)} />
            </div>

            <div class="pv-modal-actions">
              <button type="button" class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>Cancel</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._saving}>
                ${this._saving
                  ? html`<span class="spinner-border spinner-border-sm me-1"></span>Saving…`
                  : html`<i class="bi bi-check-lg me-1"></i>${isEdit ? 'Save changes' : 'Add provider'}`}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  // ── Main render ───────────────────────────────────────────────────────────

  render() {
    return html`
      <div class="pv-page">
        <div class="pv-header">
          <h2 class="pv-title">
            <i class="bi bi-plug me-2"></i>Providers
          </h2>
          <div class="pv-header-right">
            <span class="pv-header-count">${this._providers.length}</span>
            <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}>
              <i class="bi bi-plus-lg me-1"></i>Add
            </button>
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : ''}

        <div class="pv-card-list">
          ${this._providers.length === 0 ? html`
            <div class="pv-empty">
              <i class="bi bi-plug"></i>
              <p>No providers configured yet.</p>
              <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}>
                <i class="bi bi-plus-lg me-1"></i>Add your first provider
              </button>
            </div>
          ` : this._providers.map(p => this._renderCard(p))}
        </div>
      </div>

      ${this._modal ? this._renderModal() : ''}
    `;
  }
}
