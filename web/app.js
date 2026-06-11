import { AppTopbar }          from './components/topbar.js';
import { AppSidebar }         from './components/sidebar.js';
import { AppCopilot }         from './components/copilot.js';
import { LlmProvidersPage }   from './components/llm-providers.js';
import { ModelsHubPage }          from './components/models-hub.js';
import { ModelsLlmSection }       from './components/models-llm.js';
import { ModelsTranscribeSection } from './components/models-transcribe.js';
import { ModelsImageSection }     from './components/models-image.js';
import { ModelsTtsSection }       from './components/models-tts.js';
import { TasksPage }         from './components/cron-jobs.js';
import { AgentsPage }         from './components/agents.js';
import { ApprovalRulesPage }  from './components/approval-rules.js';
import { AgentInboxPage }     from './components/agent-inbox.js';
import { HomePage }           from './components/home-page.js';
import { LlmRequestsPage }   from './components/llm-requests.js';
import { LlmRequestDetail }  from './components/llm-request-detail.js';

customElements.define('app-topbar',           AppTopbar);
customElements.define('app-sidebar',          AppSidebar);
customElements.define('app-copilot',          AppCopilot);
customElements.define('llm-providers-page',   LlmProvidersPage);
customElements.define('models-hub-page',           ModelsHubPage);
customElements.define('models-llm-section',        ModelsLlmSection);
customElements.define('models-transcribe-section', ModelsTranscribeSection);
customElements.define('models-image-section',      ModelsImageSection);
customElements.define('models-tts-section',        ModelsTtsSection);
customElements.define('tasks-page',            TasksPage);
customElements.define('agents-page',          AgentsPage);
customElements.define('approval-rules-page',  ApprovalRulesPage);
customElements.define('agent-inbox-page',     AgentInboxPage);
customElements.define('home-page',            HomePage);
customElements.define('llm-requests-page',   LlmRequestsPage);
customElements.define('llm-request-detail',  LlmRequestDetail);

// Toggle the workspace placeholder when an LLM page opens/closes.
const workspace = document.getElementById('app-workspace');
window.addEventListener('llm-page-change', (e) => {
  workspace.style.display = e.detail.page ? 'none' : 'flex';
});
