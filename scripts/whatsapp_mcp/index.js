#!/usr/bin/env node
'use strict';

/**
 * WhatsApp MCP Server (JSON-RPC 2.0 over stdio)
 *
 * Uses whatsapp-web.js + puppeteer to provide read/write access to WhatsApp.
 * Session is persisted in ./secrets/whatsapp_session/ (LocalAuth).
 * QR code for first-time auth is saved as ASCII art to ./secrets/whatsapp_qr.txt
 *
 * Run `npm install` inside scripts/whatsapp_mcp/ before first use.
 *
 * Register with the agent:
 *   register_mcp(name="whatsapp", transport="stdio",
 *                command="node", args=["scripts/whatsapp_mcp/index.js"])
 */

const fs = require('fs');
const path = require('path');
const readline = require('readline');

// ── Paths ──────────────────────────────────────────────────────────────────

// __dirname = <project>/scripts/whatsapp_mcp  →  root = ../..
const PROJECT_ROOT = path.resolve(__dirname, '..', '..');
const SECRETS_DIR  = path.join(PROJECT_ROOT, 'secrets');
const DATA_DIR     = path.join(PROJECT_ROOT, 'data');
const SESSION_DIR  = path.join(SECRETS_DIR, 'whatsapp_session');
const QR_FILE      = path.join(SECRETS_DIR, 'whatsapp_qr.txt');

// ── Logging ────────────────────────────────────────────────────────────────

function log(msg) {
  process.stderr.write(`[whatsapp_mcp] ${msg}\n`);
}

// ── JSON-RPC notification helper ───────────────────────────────────────────
// Emits a server-initiated notification (no "id") to stdout.
// The Rust McpServer reader loop captures these and writes them to mcp_events.

function notify(method, params) {
  const msg = JSON.stringify({ jsonrpc: '2.0', method, params });
  process.stdout.write(msg + '\n');
}

// ── State ──────────────────────────────────────────────────────────────────

/**
 * Connection states:
 *   INITIALIZING  – client is starting up (loading session or launching browser)
 *   QR_READY      – need QR scan; QR saved to secrets/whatsapp_qr.html
 *   AUTHENTICATED – QR scanned, session being established
 *   READY         – fully connected, tools operational
 *   DISCONNECTED  – lost connection (will attempt reconnect)
 */
let state      = 'INITIALIZING';
let client     = null;
let lastQrStr  = null;   // dedup: only regenerate files when QR string changes

// ── WhatsApp Client ────────────────────────────────────────────────────────

function initClient() {
  let Client, LocalAuth;
  try {
    ({ Client, LocalAuth } = require('whatsapp-web.js'));
  } catch (e) {
    log(`ERROR: whatsapp-web.js not found. Run: cd scripts/whatsapp_mcp && npm install`);
    state = 'DISCONNECTED';
    return;
  }

  client = new Client({
    authStrategy: new LocalAuth({ dataPath: SESSION_DIR }),
    puppeteer: {
      headless: true,
      args: ['--no-sandbox', '--disable-setuid-sandbox', '--disable-dev-shm-usage'],
    },
  });

  client.on('qr', async (qr) => {
    state = 'QR_READY';

    // Dedup: skip file writes if the QR string hasn't changed.
    if (qr === lastQrStr) return;
    lastQrStr = qr;

    const QR_HTML = path.join(SECRETS_DIR, 'whatsapp_qr.html');

    // Generate a scannable HTML page with embedded PNG data-URL.
    try {
      const qrcode = require('qrcode');
      const dataUrl = await qrcode.toDataURL(qr, { width: 300, margin: 2 });
      const html = `<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>WhatsApp QR</title>
<style>body{display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:100vh;margin:0;background:#111;color:#eee;font-family:sans-serif;}
img{border:12px solid white;border-radius:8px;}</style></head>
<body>
  <h2>Scan with WhatsApp → Linked Devices → Link a Device</h2>
  <img src="${dataUrl}" alt="WhatsApp QR Code" />
  <p style="margin-top:1rem;opacity:.6">This QR expires in ~20 s — reload the page if it has changed</p>
</body>
</html>`;
      fs.writeFileSync(QR_HTML, html, 'utf8');
      log(`QR code saved → open in browser: ${QR_HTML}`);

      // Also save a standalone PNG for direct access via HTTP / local file.
      const pngPath = path.join(DATA_DIR, 'whatsapp_qr.png');
      await qrcode.toFile(pngPath, qr, { width: 300, margin: 2 });
      log(`QR PNG saved → ${pngPath}`);
    } catch (e) {
      // Fallback: ASCII art text file
      try {
        const qrTerm = require('qrcode-terminal');
        let qrText = '';
        qrTerm.generate(qr, { small: true }, (str) => { qrText = str; });
        fs.writeFileSync(QR_FILE, qrText, 'utf8');
        log(`QR code (ASCII) saved to ${QR_FILE} — scan it with WhatsApp`);
      } catch (_) {
        fs.writeFileSync(QR_FILE, `RAW_QR_STRING:\n${qr}`, 'utf8');
        log(`QR raw string saved to ${QR_FILE}`);
      }
    }
  });

  client.on('authenticated', () => {
    state = 'AUTHENTICATED';
    lastQrStr = null;
    const QR_HTML = path.join(SECRETS_DIR, 'whatsapp_qr.html');
    const QR_PNG  = path.join(DATA_DIR, 'whatsapp_qr.png');
    if (fs.existsSync(QR_FILE))  fs.unlinkSync(QR_FILE);
    if (fs.existsSync(QR_HTML))  fs.unlinkSync(QR_HTML);
    if (fs.existsSync(QR_PNG))   fs.unlinkSync(QR_PNG);
    log('Authenticated successfully');
  });

  client.on('ready', () => {
    state = 'READY';
    log('WhatsApp client ready');
  });

  // ── Push notifications: new incoming messages ──────────────────────────
  client.on('message', async (msg) => {
    // Ignore messages sent by us.
    if (msg.fromMe) return;

    // Try to resolve the human-readable chat name.
    let chatName = msg.from;
    try {
      const chat = await msg.getChat();
      chatName = chat.name || msg.from;
    } catch (_) {}

    notify('event/whatsapp_message', {
      chat_id:   msg.from,
      chat_name: chatName,
      from:      msg.author || msg.from,
      body:      (msg.body || '').slice(0, 1000),
      timestamp: msg.timestamp,
      is_group:  msg.from.endsWith('@g.us'),
    });
    log(`Notification emitted: new message from ${chatName}`);
  });

  client.on('auth_failure', (msg) => {
    state = 'DISCONNECTED';
    log(`Auth failure: ${msg}`);
  });

  client.on('disconnected', (reason) => {
    state = 'DISCONNECTED';
    log(`Disconnected: ${reason}`);
  });

  client.initialize().catch((e) => {
    log(`Failed to initialize client: ${e.message}`);
    state = 'DISCONNECTED';
  });
}

// ── Helpers ────────────────────────────────────────────────────────────────

function requireReady() {
  if (state !== 'READY') {
    return `WhatsApp not ready (status: ${state}).${
      state === 'QR_READY'
        ? ' Use whatsapp_get_qr to retrieve the QR code and scan it with your phone.'
        : state === 'INITIALIZING'
        ? ' Please wait a moment and try again.'
        : ''
    }`;
  }
  return null;
}

function formatTimestamp(unixSec) {
  return new Date(unixSec * 1000).toISOString().replace('T', ' ').slice(0, 19);
}

// ── Tool implementations ───────────────────────────────────────────────────

async function toolStatus() {
  const qrNote = state === 'QR_READY'
    ? '\nQR code available at secrets/whatsapp_qr.txt — use whatsapp_get_qr to retrieve it.'
    : '';
  return `Status: ${state}${qrNote}`;
}

async function toolGetQr() {
  if (state === 'READY' || state === 'AUTHENTICATED') {
    return 'Already authenticated. No QR code needed.';
  }
  const QR_PNG = path.join(DATA_DIR, 'whatsapp_qr.png');
  if (fs.existsSync(QR_PNG)) {
    return `QR code ready.
• Local file: ${QR_PNG}
• URL: /data/whatsapp_qr.png
Open the URL in your browser or scan locally.

Current status: ${state}`;
  }
  const QR_HTML = path.join(SECRETS_DIR, 'whatsapp_qr.html');
  if (fs.existsSync(QR_HTML)) {
    return `Open this file in your browser to scan the QR code:\n  ${QR_HTML}\n\nThe page shows a scannable QR image. Go to WhatsApp → Settings → Linked Devices → Link a Device.\n\nCurrent status: ${state}`;
  }
  if (fs.existsSync(QR_FILE)) {
    const qr = fs.readFileSync(QR_FILE, 'utf8');
    return `Scan this QR code with WhatsApp (Settings → Linked Devices → Link a Device):\n\n${qr}`;
  }
  return `No QR code available yet. Current status: ${state}. The client may still be initializing — try again in a few seconds.`;
}

async function toolListChats(args) {
  const err = requireReady(); if (err) return err;
  const max = Math.min(args.max_chats || 20, 50);
  const chats = await client.getChats();
  const slice = chats.slice(0, max);
  const lines = [`Chats (${slice.length} of ${chats.length} total):`];
  for (const chat of slice) {
    const unread = chat.unreadCount > 0 ? ` [${chat.unreadCount} unread]` : '';
    const type   = chat.isGroup ? '[group]' : '[contact]';
    lines.push(`- ${chat.name} ${type}${unread}`);
    lines.push(`  ID: ${chat.id._serialized}`);
  }
  return lines.join('\n');
}

async function toolGetMessages(args) {
  const err = requireReady(); if (err) return err;
  const chatId = args.chat_id;
  if (!chatId) return 'Error: Missing required parameter chat_id.';
  const limit  = Math.min(args.limit  || 20,  100);
  const offset = Math.max(args.offset ||  0,    0);
  const chat   = await client.getChatById(chatId);

  // fetchMessages always returns the most recent N messages (oldest→newest).
  // To support paging, we fetch limit+offset and discard the newest `offset`
  // messages, exposing the preceding window.
  //   offset=0,  limit=20 → last 20 messages
  //   offset=20, limit=20 → messages 21–40 from the end (older batch)
  const toFetch  = Math.min(limit + offset, 200);
  const fetched  = await chat.fetchMessages({ limit: toFetch });
  const windowed = offset > 0 ? fetched.slice(0, fetched.length - offset) : fetched;
  const page     = windowed.slice(-limit);

  if (!page.length) return offset > 0
    ? `No messages found at offset ${offset} (only ${fetched.length} messages available).`
    : 'No messages found.';

  const rangeNote = offset > 0 ? `, skipping newest ${offset}` : '';
  const lines = [`Messages from "${chat.name}" (${page.length} shown, limit=${limit}${rangeNote}):`];
  for (const msg of page) {
    const ts     = formatTimestamp(msg.timestamp);
    const author = msg.fromMe ? 'Me' : (msg.author || msg.from || '?');
    const body   = (msg.body || '(media/no text)').slice(0, 400);
    lines.push(`[${ts}] ${author}: ${body}`);
  }
  return lines.join('\n');
}

async function toolSendMessage(args) {
  const err = requireReady(); if (err) return err;
  const { chat_id, message } = args;
  if (!chat_id)  return 'Error: Missing required parameter chat_id.';
  if (!message)  return 'Error: Missing required parameter message.';
  const chat = await client.getChatById(chat_id);
  await chat.sendMessage(message);
  return `✅ Message sent to "${chat.name}"`;
}

async function toolSearchMessages(args) {
  const err = requireReady(); if (err) return err;
  const { query } = args;
  if (!query) return 'Error: Missing required parameter query.';
  const max = Math.min(args.max_results || 20, 50);
  const messages = await client.searchMessages(query, { limit: max });
  if (!messages.length) return `No messages found for query: "${query}"`;
  const lines = [`Search results for "${query}" (${messages.length} found):`];
  for (const msg of messages) {
    const ts   = formatTimestamp(msg.timestamp);
    const from = msg.fromMe ? 'Me' : msg.from;
    const body = (msg.body || '(media)').slice(0, 200);
    lines.push(`[${ts}] ${from} (chat: ${msg.id.remote}): ${body}`);
  }
  return lines.join('\n');
}

async function toolSearchContacts(args) {
  const err = requireReady(); if (err) return err;
  const { query } = args;
  if (!query) return 'Error: Missing required parameter query. Provide a name or partial name to search for.';
  const max = Math.min(args.max_results || 20, 50);
  const contacts = await client.getContacts();
  const q = query.toLowerCase();

  // Deduplicate by serialized ID, then filter by name match, exclude self.
  const seen = new Set();
  const matches = [];
  for (const c of contacts) {
    if (!c.name || c.isMe) continue;
    const id = c.id._serialized;
    if (seen.has(id)) continue;
    seen.add(id);
    if (c.name.toLowerCase().includes(q)) matches.push(c);
    if (matches.length >= max) break;
  }

  if (!matches.length) return `No contacts found matching "${query}".`;
  const lines = [`Contacts matching "${query}" (${matches.length} shown):`];
  for (const c of matches) {
    const type = c.isGroup ? '[group]' : c.isBusiness ? '[business]' : '[contact]';
    lines.push(`- ${c.name} ${type} | ID: ${c.id._serialized}`);
  }
  return lines.join('\n');
}

// ── MCP Tool definitions ───────────────────────────────────────────────────

const TOOLS = [
  {
    name: 'whatsapp_status',
    description: 'Get the current WhatsApp connection status. Returns INITIALIZING, QR_READY (scan needed), AUTHENTICATED, READY, or DISCONNECTED.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'whatsapp_get_qr',
    description: 'Get the QR code ASCII art for WhatsApp authentication. Only relevant when status is QR_READY. Show the output verbatim so the user can scan it with the WhatsApp app on their phone.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'whatsapp_list_chats',
    description: 'List recent WhatsApp chats (individual contacts and groups) with name, ID, and unread count. Use the returned ID in other tools.',
    inputSchema: {
      type: 'object',
      properties: {
        max_chats: {
          type: 'integer',
          description: 'Max chats to return (default 20, max 50).',
        },
      },
    },
  },
  {
    name: 'whatsapp_get_messages',
    description: 'Get messages from a specific WhatsApp chat or group by chat ID. ' +
      'Use whatsapp_list_chats first to get the chat_id. ' +
      'Supports pagination: use offset to skip the most recent messages and read older history. ' +
      'Example: limit=20 offset=0 → last 20; limit=20 offset=20 → previous 20; limit=20 offset=40 → older 20.',
    inputSchema: {
      type: 'object',
      properties: {
        chat_id: {
          type: 'string',
          description: 'The chat ID (e.g. "39xxxxxxxxxx@c.us" for a contact, "xxxxxxxxxx-xxxxxxxxxx@g.us" for a group).',
        },
        limit: {
          type: 'integer',
          description: 'Number of messages to return (default 20, max 100).',
        },
        offset: {
          type: 'integer',
          description: 'Number of recent messages to skip before returning results (default 0). ' +
            'Increment by `limit` to page through older history.',
        },
      },
      required: ['chat_id'],
    },
  },
  {
    name: 'whatsapp_send_message',
    description: 'Send a WhatsApp text message to a chat or group.',
    inputSchema: {
      type: 'object',
      properties: {
        chat_id: {
          type: 'string',
          description: 'The chat ID to send to (from whatsapp_list_chats).',
        },
        message: {
          type: 'string',
          description: 'The text message to send.',
        },
      },
      required: ['chat_id', 'message'],
    },
  },
  {
    name: 'whatsapp_search_messages',
    description: 'Search messages across all WhatsApp chats by keyword.',
    inputSchema: {
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description: 'Keyword to search for in message content.',
        },
        max_results: {
          type: 'integer',
          description: 'Max results to return (default 20, max 50).',
        },
      },
      required: ['query'],
    },
  },
  {
    name: 'whatsapp_search_contacts',
    description: 'Search saved WhatsApp contacts by name. Use this to find the ID of a contact ' +
      'you want to message but who does not appear in recent chats. ' +
      'For conversations already open, use whatsapp_list_chats instead.',
    inputSchema: {
      type: 'object',
      properties: {
        query: {
          type: 'string',
          description: 'Name or partial name to search for (case-insensitive).',
        },
        max_results: {
          type: 'integer',
          description: 'Max contacts to return (default 20, max 50).',
        },
      },
      required: ['query'],
    },
  },
];

// ── JSON-RPC helpers ───────────────────────────────────────────────────────

function okResponse(id, result) {
  return JSON.stringify({ jsonrpc: '2.0', id, result });
}

function textResult(id, text, isError = false) {
  const result = { content: [{ type: 'text', text }] };
  if (isError) result.isError = true;
  return JSON.stringify({ jsonrpc: '2.0', id, result });
}

// ── Request dispatch ───────────────────────────────────────────────────────

async function handleRequest(msg) {
  const { method, id, params } = msg;

  if (method === 'initialize') {
    return okResponse(id, {
      protocolVersion: '2024-11-05',
      capabilities: { tools: {} },
      serverInfo: { name: 'whatsapp', version: '1.0.0' },
    });
  }

  if (method === 'notifications/initialized') return null;

  if (method === 'tools/list') {
    return okResponse(id, { tools: TOOLS });
  }

  if (method === 'tools/call') {
    const toolName = params?.name || '';
    const toolArgs = params?.arguments || {};
    let text;
    try {
      switch (toolName) {
        case 'whatsapp_status':          text = await toolStatus(toolArgs);         break;
        case 'whatsapp_get_qr':          text = await toolGetQr(toolArgs);          break;
        case 'whatsapp_list_chats':      text = await toolListChats(toolArgs);      break;
        case 'whatsapp_get_messages':    text = await toolGetMessages(toolArgs);    break;
        case 'whatsapp_send_message':    text = await toolSendMessage(toolArgs);    break;
        case 'whatsapp_search_messages': text = await toolSearchMessages(toolArgs); break;
        case 'whatsapp_search_contacts': text = await toolSearchContacts(toolArgs); break;
        default:
          return textResult(id, `Unknown tool: ${toolName}`, true);
      }
    } catch (e) {
      log(`Unhandled error in tool '${toolName}': ${e.message}`);
      return textResult(id, `Error: Internal error in '${toolName}': ${e.message}`, true);
    }
    const isErr = text.startsWith('Error:');
    return textResult(id, text, isErr);
  }

  return JSON.stringify({
    jsonrpc: '2.0', id,
    error: { code: -32601, message: `Method not found: ${method}` },
  });
}

// ── Main ───────────────────────────────────────────────────────────────────

async function main() {
  log('Starting WhatsApp MCP server');
  log(`Session dir: ${SESSION_DIR}`);

  if (!fs.existsSync(SECRETS_DIR)) {
    fs.mkdirSync(SECRETS_DIR, { recursive: true });
  }
  if (!fs.existsSync(DATA_DIR)) {
    fs.mkdirSync(DATA_DIR, { recursive: true });
  }

  initClient();

  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

  rl.on('line', async (line) => {
    line = line.trim();
    if (!line) return;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch (e) {
      log(`Invalid JSON on stdin: ${e.message}`);
      return;
    }
    const resp = await handleRequest(msg);
    if (resp !== null) {
      process.stdout.write(resp + '\n');
    }
  });

  rl.on('close', () => {
    log('stdin closed, shutting down');
    if (client) client.destroy().catch(() => {});
    process.exit(0);
  });
}

main().catch((e) => {
  log(`Fatal: ${e.message}`);
  process.exit(1);
});
