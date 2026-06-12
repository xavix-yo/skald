import { LightElement } from './base.js';

/**
 * Base class for chat UI components (desktop copilot, mobile chat page).
 *
 * Contains all WebSocket logic, message state, and approval handling.
 * Subclasses implement the render() method and override the DOM hooks:
 *   - _scrollToBottom()
 *   - _getInputContent()  → returns current input value
 *   - _clearInput()       → empties the input
 *   - _onMessagePushed(item) → called after each push (scroll, focus, etc.)
 */
export class ChatSession extends LightElement {
  static properties = {
    _messages:           { state: true },
    _waiting:            { state: true },
    _expanded:           { state: true },
    _providers:          { state: true },
    _selectedClient:     { state: true },
    _rejectingId:           { state: true },
    _rejectNote:            { state: true },
    _clarificationAnswer:   { state: true },
  };

  constructor() {
    super();
    this._messages          = [];
    this._waiting           = false;
    this._expanded          = new Set();
    this._ws                = null;
    this._providers         = [];
    this._selectedClient    = null;
    this._rejectingId           = null;
    this._rejectNote            = '';
    this._clarificationAnswer   = '';
  }

  async connectedCallback() {
    super.connectedCallback();
    await Promise.all([this._loadProviders(), this._loadHistory()]);
    this._connectWS();
  }

  // ── Source identity — override in subclass ────────────────────────────────────

  get _wsSource() { return 'web'; }

  // ── Data loading ──────────────────────────────────────────────────────────────

  async _loadProviders() {
    try {
      const res = await fetch('/api/llm/models/selector');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const { models, default: def } = await res.json();
      this._providers      = models;
      this._selectedClient = def;
    } catch (e) {
      console.error('Failed to load LLM models:', e);
    }
  }

  async _loadHistory() {
    try {
      const res = await fetch(`/api/${this._wsSource}/messages`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const items = await res.json();
      if (items.length > 0) {
        this._messages = items;
        const expanded = new Set(this._expanded);
        for (const m of items) {
          if (m.kind === 'tool' && m.status === 'pending') expanded.add(m.tool_call_id);
        }
        this._expanded = expanded;
        this._scrollToBottom();
        // Set flag so ws.onopen sends a resume if there are pending tools
        // (approval/clarification waiting) or interrupted tools (status=error+Interrupted).
        this._hasPendingTools = items.some(
          m => m.kind === 'tool' && (m.status === 'pending' || (m.status === 'error' && m.error === 'Interrupted.'))
        );
      }
    } catch (e) {
      console.warn('Could not load history:', e.message);
    }
  }

  // ── WebSocket ─────────────────────────────────────────────────────────────────

  _connectWS() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/api/ws?source=${this._wsSource}`);
    this._ws = ws;
    ws.onopen = () => {
      if (this._hasPendingTools) {
        ws.send(JSON.stringify({ type: 'resume' }));
        this._hasPendingTools = false;
      }
    };
    ws.onmessage = (ev) => this._handleServerMsg(JSON.parse(ev.data));
    ws.onclose   = ()   => setTimeout(() => this._connectWS(), 2000);
  }

  async _startNewSession() {
    if (this._ws) {
      this._ws.onclose = null;
      this._ws.close();
      this._ws = null;
    }
    this._messages = [];
    this._waiting  = false;
    try {
      const res = await fetch(`/api/sessions?source=${this._wsSource}`, { method: 'POST' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
    } catch (e) {
      this._pushError('Could not clear session: ' + e.message);
    }
    this._connectWS();
  }

  // ── Message handling ──────────────────────────────────────────────────────────

  _handleServerMsg(msg) {
    console.debug('[WS ←]', msg.type, msg);
    switch (msg.type) {
      case 'pending_write':
        this._push({
          kind:         'pending_write',
          request_id:   msg.request_id,
          tool_call_id: msg.tool_call_id,
          path:         msg.path,
          old_content:  msg.old_content ?? '',
          new_content:  msg.new_content,
          status:       'pending',
        });
        break;

      case 'thinking':
        this._push({ kind: 'thinking', message_id: msg.message_id, content: msg.content,
                     input_tokens: msg.input_tokens, output_tokens: msg.output_tokens });
        break;

      case 'done':
        this._waiting = false;
        this._push({ kind: 'assistant', content: msg.content,
                     input_tokens: msg.input_tokens, output_tokens: msg.output_tokens });
        break;

      case 'tool_start': {
        // On resume, the server re-emits ToolStart for tools already in history.
        // Update in place rather than pushing a duplicate card.
        const existingIdx = this._messages.findIndex(
          m => (m.kind === 'tool') && m.tool_call_id === msg.tool_call_id
        );
        if (existingIdx >= 0) {
          this._updateTool(msg.tool_call_id, { status: 'running', result: null, error: null });
        } else {
          this._push({
            kind:         'tool',
            tool_call_id: msg.tool_call_id,
            name:         msg.name,
            label_short:  msg.label_short,
            label_full:   msg.label_full,
            arguments:    msg.arguments,
            status:       'running',
            result:       null,
            error:        null,
          });
        }
        break;
      }

      case 'tool_done':
        this._updateTool(msg.tool_call_id, { status: 'done', result: msg.result });
        break;

      case 'tool_error':
        this._updateTool(msg.tool_call_id, { status: 'error', error: msg.error });
        break;

      case 'approval_required':
        this._updateTool(msg.tool_call_id, { status: 'pending', request_id: msg.request_id });
        this._expanded = new Set([...this._expanded, msg.tool_call_id]);
        this.updateComplete.then(() => this._scrollToBottom());
        break;

      case 'approval_resolved': {
        const { request_id, tool_call_id, approved } = msg;
        this._updatePendingWrite(request_id, { status: approved ? 'approved' : 'rejected' });
        if (tool_call_id != null) {
          if (approved) {
            this._updateTool(tool_call_id, { status: 'running', request_id: null });
          } else {
            this._updateTool(tool_call_id, { status: 'error', error: 'Rifiutato.' });
          }
          const expanded = new Set(this._expanded);
          expanded.delete(tool_call_id);
          this._expanded = expanded;
        }
        break;
      }

      case 'agent_question':
        // Link the question form to the tool card by updating status + storing request_id.
        this._updateTool(msg.tool_call_id, {
          status:            'pending',
          request_id:        msg.request_id,
          question:          msg.question,
          question_title:    msg.title,
          suggested_answers: msg.suggested_answers ?? [],
        });
        this._expanded = new Set([...this._expanded, msg.tool_call_id]);
        this.updateComplete.then(() => this._scrollToBottom());
        break;

      case 'agent_start':
        this._push({
          kind:            'agent',
          stack_id:        msg.stack_id,
          agent_id:        msg.agent_id,
          parent_agent_id: msg.parent_agent_id,
          prompt_preview:  msg.prompt_preview,
          depth:           msg.depth,
          done:            false,
        });
        break;

      case 'agent_done': {
        this._updateAgent(msg.stack_id, { done: true });
        const agentMsg = this._messages.find(m => m.kind === 'agent' && m.stack_id === msg.stack_id);
        if (agentMsg) {
          this._push({
            kind:            'agent_end',
            agent_id:        msg.agent_id,
            parent_agent_id: msg.parent_agent_id,
            result_preview:  msg.result_preview,
            depth:           agentMsg.depth,
          });
        }
        break;
      }

      case 'truncated':
        this._pushError(`Risposta troncata dal limite di token (↓${msg.output_tokens?.toLocaleString() ?? '?'} tok).`);
        break;

      case 'error':
        this._waiting = false;
        this._pushError(msg.message);
        break;

      case 'file_changed':
        window.dispatchEvent(new CustomEvent('file-changed', { detail: { path: msg.path } }));
        break;

      case 'model_fallback':
        this._push({ kind: 'info', content: `⚡ Model fallback: ${msg.from} → ${msg.to}` });
        break;

      case 'user_message': {
        // Deduplicate: the sending client already pushed this locally in _send().
        // Other clients (different tabs, mobile) push it here.
        const last = this._messages[this._messages.length - 1];
        if (!(last?.kind === 'user' && last.content === msg.content)) {
          this._push({ kind: 'user', content: msg.content });
        }
        break;
      }

      case 'new_session':
        this._messages = [];
        this._waiting  = false;
        break;

      case 'llm_failed':
        this._waiting = false;
        this._pushError(`LLM unavailable. Tried: ${msg.tried.join(', ')}. ${msg.last_error}`);
        break;
    }
  }

  _push(item) {
    console.debug('[push]', item.kind, item);
    this._messages = [...this._messages, item];
    this._onMessagePushed(item);
  }

  _pushError(text) {
    this._push({ kind: 'error', content: text });
  }

  _updateAgent(stack_id, patch) {
    const idx = this._messages.findIndex(m => m.kind === 'agent' && m.stack_id === stack_id);
    if (idx < 0) return;
    const updated = [...this._messages];
    updated[idx] = { ...updated[idx], ...patch };
    this._messages = updated;
  }

  _updateTool(tool_call_id, patch) {
    const idx = this._messages.findIndex(
      m => (m.kind === 'tool' || m.kind === 'pending_write') && m.tool_call_id === tool_call_id
    );
    if (idx < 0) return;
    const updated = [...this._messages];
    updated[idx] = { ...updated[idx], ...patch };
    this._messages = updated;
  }

  _updatePendingWrite(request_id, patch) {
    const idx = this._messages.findIndex(
      m => m.kind === 'pending_write' && m.request_id === request_id
    );
    if (idx < 0) return;
    // Once resolved (approved or rejected) remove the block entirely —
    // the tool card already shows the outcome.
    if (patch.status === 'approved' || patch.status === 'rejected') {
      this._messages = this._messages.filter((_, i) => i !== idx);
      return;
    }
    const updated = [...this._messages];
    updated[idx] = { ...updated[idx], ...patch };
    this._messages = updated;
  }

  // ── User input ────────────────────────────────────────────────────────────────

  async _send() {
    const content = this._getInputContent();
    if (!content || this._waiting) return;
    this._clearInput();

    if (content === '/new' || content === '/clear') {
      await this._startNewSession();
      return;
    }

    this._push({ kind: 'user', content });
    this._waiting = true;

    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({
        content,
        client: this._selectedClient,
      }));
    } else {
      this._pushError('Not connected — reconnecting, please retry.');
      this._waiting = false;
    }
  }

  _cancel() {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'cancel' }));
    }
    this._waiting = false;
  }

  // ── Approval — pending_write (WS) ─────────────────────────────────────────────

  _approve(msg) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'approve_write', request_id: msg.request_id }));
    }
    this._updatePendingWrite(msg.request_id, { status: 'approved' });
  }

  _approveWriteBypass(msg, bypassSecs) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'approve_write', request_id: msg.request_id, bypass_secs: bypassSecs }));
    }
    this._updatePendingWrite(msg.request_id, { status: 'approved' });
  }

  _startReject(msg) {
    this._rejectingId = msg.request_id;
    this._rejectNote  = '';
  }

  _confirmReject(msg) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'reject_write', request_id: msg.request_id, note: this._rejectNote }));
    }
    this._updatePendingWrite(msg.request_id, { status: 'rejected' });
    this._rejectingId = null;
  }

  // ── Approval — tool (WS, live) ────────────────────────────────────────────────

  _approveWsTool(msg) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'approve_tool', request_id: msg.request_id }));
    }
    this._updateTool(msg.tool_call_id, { status: 'running', request_id: null });
    this._rejectingId = null;
  }

  _approveWsToolBypass(msg, bypassSecs) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'approve_tool', request_id: msg.request_id, bypass_secs: bypassSecs }));
    }
    this._updateTool(msg.tool_call_id, { status: 'running', request_id: null });
    this._rejectingId = null;
  }

  _rejectWsTool(msg) {
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'reject_tool', request_id: msg.request_id, note: this._rejectNote }));
    }
    this._updateTool(msg.tool_call_id, { status: 'error', error: "Rifiutato dall'utente." });
    this._rejectingId = null;
  }

  // ── Approval — tool (REST, from history) ─────────────────────────────────────

  async _approveTool(msg) {
    this._updateTool(msg.tool_call_id, { status: 'running' });
    try {
      const res = await fetch(`/api/web/tools/${msg.tool_call_id}/resolve`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ action: 'approve' }),
      });
      if (!res.ok) {
        this._updateTool(msg.tool_call_id, { status: 'error', error: `Approval failed: ${await res.text()}` });
        return;
      }
      const data = await res.json();
      this._updateTool(msg.tool_call_id, { status: data.status, result: data.result });
      if (this._ws?.readyState === WebSocket.OPEN) this._ws.send(JSON.stringify({ type: 'resume' }));
    } catch (e) {
      this._updateTool(msg.tool_call_id, { status: 'error', error: String(e) });
    }
  }

  async _rejectTool(msg) {
    this._updateTool(msg.tool_call_id, { status: 'running' });
    try {
      const res = await fetch(`/api/web/tools/${msg.tool_call_id}/resolve`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ action: 'reject', note: this._rejectNote }),
      });
      if (!res.ok) {
        this._updateTool(msg.tool_call_id, { status: 'error', error: `Rejection failed: ${await res.text()}` });
        return;
      }
      this._updateTool(msg.tool_call_id, { status: 'error', error: 'Rejected by user.' });
      this._rejectingId = null;
      if (this._ws?.readyState === WebSocket.OPEN) this._ws.send(JSON.stringify({ type: 'resume' }));
    } catch (e) {
      this._updateTool(msg.tool_call_id, { status: 'error', error: String(e) });
    }
  }

  // ── Clarification ─────────────────────────────────────────────────────────────

  _answerQuestion(msg) {
    const answer = this._clarificationAnswer.trim();
    if (!answer) return;
    if (this._ws?.readyState === WebSocket.OPEN) {
      this._ws.send(JSON.stringify({ type: 'answer_question', request_id: msg.request_id, answer }));
    }
    this._updateTool(msg.tool_call_id, { status: 'running', request_id: null });
    this._clarificationAnswer = '';
  }

  // ── DOM hooks (override in subclass) ─────────────────────────────────────────

  /** Called after every _push(). Override to handle scrolling, focus, etc. */
  _onMessagePushed(_item) {}

  /** Returns the current value of the chat input. */
  _getInputContent() { return ''; }

  /** Clears the chat input. */
  _clearInput() {}

  /** Scrolls the message list to the bottom. */
  _scrollToBottom() {}
}
