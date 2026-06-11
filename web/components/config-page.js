import { html } from 'lit';
import { LightElement } from '../lib/base.js';

export class ConfigPage extends LightElement {
  static properties = {
    _open: { state: true },
  };

  constructor() {
    super();
    this._open = false;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'config';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
  }

  async _load() {
  }

  render() {
    return html`
      <div class="config-page">
        <div class="config-page-header">
          <h2 class="llm-page-title">Config</h2>
        </div>
      </div>
    `;
  }
}
