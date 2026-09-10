const spinAssetBase = new URL('.', document.currentScript.src).href.replace(/\/$/, '');
let snapshot = {artifacts:[],recordings:[],compositions:[],jobs:[],job_attachments:[],workflow_templates:[],phase_runs:[],deliverables:[],deliverable_comments:[],code_review_revisions:[],code_review_comments:[],workflow_questions:[],sessions:[],activations:[],turns:[],checkpoints:[],results:[],clients:[],mcp_servers:[],git_repositories:[],git_accounts:[],git_oauth_providers:[],users:[],recommendations:[]};
let authState = {configured:false,authenticated:false,user:null};
let csrfToken = '';
const spawnDrafts = new Map();
let templateStepSequence = 0, editingTemplateID = '', editingGitRepositoryID = '';
let jobSubmitting = false, pendingJobSubmission = null, attachmentTargetJobID = '', forkingJobID = '';
let jobStateFilter = ['mine','all','closed'].includes(localStorage.getItem('spin-job-state'))?localStorage.getItem('spin-job-state'):'mine';
let jobSearch = '';
// Closed work is found by name or reference: title, ticket, branch.
const jobMatchesSearch = (job,query) => !query||[job.title,job.reference,job.branch].some(value=>String(value||'').toLowerCase().includes(query));
let pendingRejectQuestionID = '';
const detailStates = new Map();
const terminalSessions = new Map();
let activeTerminalID = null, terminalSequence = 0;
const deliverableView = {id:'',selection:null,ranges:new Map()};
const chatState = {sessionID:'',socket:null,busy:false,manualClose:false,followTail:true,messageNodes:new Map(),toolNodes:new Map(),thoughtNodes:new Map(),metaNodes:new Map(),planNode:null,changeTimer:null,reconnectTimer:null,changes:{branch:'',added:0,deleted:0,files:[]},selectedDiffPath:''};
const jobChangesState = {jobID:'',sessionID:'',changes:{branch:'',added:0,deleted:0,files:[]},selectedPath:'',bundle:null,selection:null};

const esc = value => String(value ?? '').replace(/[&<>'"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;',"'":'&#39;','"':'&quot;'}[c]));
const icon = name => `<span class="material-symbols-outlined" aria-hidden="true">${esc(name)}</span>`;
const normalize = value => String(value || '').trim().toLowerCase();
const short = value => esc(value || '—').slice(0,20);
const formatDateTime = value => {
  const date=new Date(value);if(!value||Number.isNaN(date.getTime()))return '';
  return new Intl.DateTimeFormat('nl-NL',{dateStyle:'medium',timeStyle:'short'}).format(date);
};
const elapsedSince = value => {
  const started=new Date(value),seconds=Math.max(0,Math.floor((Date.now()-started.getTime())/1000));if(!value||Number.isNaN(started.getTime()))return 'onbekend';
  if(seconds<60)return `${seconds}s`;if(seconds<3600)return `${Math.floor(seconds/60)}m ${seconds%60}s`;if(seconds<86400)return `${Math.floor(seconds/3600)}u ${Math.floor(seconds%3600/60)}m`;return `${Math.floor(seconds/86400)}d ${Math.floor(seconds%86400/3600)}u`;
};
const byID = (items,id) => items.find(item=>item.id===id);
const currentOperator = () => normalize(authState.user?.username) || 'anonymous';
// A version that a newer one replaced stays in state for the layers built on it,
// but it is no longer something to pick.
const canUse = artifact => !artifact.superseded_by && (artifact.scope !== 'user' || artifact.subject === currentOperator());
const enabledNames = values => (Array.isArray(values)?values:[]).map(value=>value.name).join(', ');
const selectedValues = select => [...select.selectedOptions].map(option=>option.value).filter(Boolean);
const detailOpenAttribute = (key,defaultOpen=false) => (detailStates.has(key)?detailStates.get(key):defaultOpen)?'open':'';
function bindDetailStates(root){root.querySelectorAll('details[data-detail-state]').forEach(detail=>detail.ontoggle=()=>detailStates.set(detail.dataset.detailState,detail.open));}
const artifactSelector = artifact => `${artifact.kind}:${artifact.name}`;
// A newer version sits on its predecessor, but it is the same
// layer: the depth counts distinct slots, and the versions are counted apart.
const artifactLayer = (artifact,seen=new Set()) => {
  if(!artifact || seen.has(artifact.id)) return 0;
  const next=new Set(seen); next.add(artifact.id);
  const parents=(artifact.parent_artifact_ids||[]).map(id=>byID(snapshot.artifacts,id)).filter(Boolean);
  return parents.length?Math.max(...parents.map(parent=>(parent.slot&&parent.slot===artifact.slot?0:1)+artifactLayer(parent,next))):1;
};
const artifactLineage = artifact => {
  let version=1,base=artifact;const seen=new Set();
  while(base&&!seen.has(base.id)){seen.add(base.id);const previous=(base.parent_artifact_ids||[]).map(id=>byID(snapshot.artifacts,id)).find(parent=>parent&&parent.slot&&parent.slot===base.slot);if(!previous)break;version+=1;base=previous;}
  const parents=(base?.parent_artifact_ids||[]).map(id=>byID(snapshot.artifacts,id)?.name||id);
  return {version,parents};
};
const artifactEnables = (artifact,capability,seen=new Set()) => {
  if(!artifact||seen.has(artifact.id))return false;
  if((artifact.enables||[]).some(item=>item.name===capability))return true;
  const next=new Set(seen);next.add(artifact.id);
  return (artifact.parent_artifact_ids||[]).some(id=>artifactEnables(byID(snapshot.artifacts,id),capability,next));
};
const artifactDirectlyEnables = (artifact,capability) => Boolean(artifact&&(artifact.enables||[]).some(item=>item.name===capability));
const capabilitySelectors = capability => [...new Set(snapshot.artifacts.filter(artifact=>canUse(artifact)&&artifactEnables(artifact,capability)).map(artifactSelector))];
const capabilityProviderSelectors = capability => [...new Set(snapshot.artifacts.filter(artifact=>canUse(artifact)&&artifactDirectlyEnables(artifact,capability)).map(artifactSelector))];
const projectLayerSelectors = () => [...new Set(snapshot.artifacts.filter(artifact=>canUse(artifact)&&artifact.kind!=='credential'&&!artifactDirectlyEnables(artifact,'git')&&!artifactDirectlyEnables(artifact,'acp')).map(artifactSelector))];
async function api(path,options={}) {
  const method=String(options.method||'GET').toUpperCase(),multipart=typeof FormData!=='undefined'&&options.body instanceof FormData,headers={...(options.body&&!multipart?{'Content-Type':'application/json'}:{}),...(options.headers||{})};
  if(csrfToken&&['POST','PUT','PATCH','DELETE'].includes(method))headers['X-Spin-CSRF']=csrfToken;
  const response=await fetch(path,{...options,headers,credentials:'same-origin'});
  if(!response.ok){
    const body=await response.json().catch(()=>({error:response.statusText}));
    // A restarted server rotates the CSRF token; pick the new one up once and retry.
    if(response.status===403&&/CSRF/.test(body.error||'')&&!options.retried){const status=await api('/api/auth/status');if(status?.csrf_token){authState=status;csrfToken=status.csrf_token;return api(path,{...options,retried:true});}}
    const error=new Error(body.error||response.statusText);error.status=response.status;error.body=body;throw error;
  }
  return response.status===204?null:response.json();
}
function openDialog(id){const dialog=document.getElementById(id);if(dialog&&!dialog.open)dialog.showModal();}
function closeDialog(id){const dialog=document.getElementById(id);if(dialog?.open)dialog.close();}
// The recorder has no command line: a RECORD is a form, END and CANCEL are
// buttons, and the terminal is the capsule's shell. The log says in plain
// words what was done.
const recordPresets=[
  {label:'tool:git',kind:'tool',name:'git',scope:'global',from:'',git:true},
  {label:'tool:node',kind:'tool',name:'node',scope:'global',from:'tool:git'},
  {label:'tool:codex + ACP',kind:'tool',name:'codex',scope:'global',from:'tool:node',acp:true,command:'codex-acp'},
  {label:'tool:claude + ACP',kind:'tool',name:'claude',scope:'global',from:'tool:node',acp:true,command:'claude-agent-acp'},
  {label:'credential:codex',kind:'credential',name:'codex',scope:'user',from:'tool:codex'},
];
function fillRecordForm(preset={}){const form=document.getElementById('record-form');form.reset();form.elements.kind.value=preset.kind||'tool';form.elements.name.value=preset.name||'';form.elements.scope.value=preset.scope||'user';renderRecordFromOptions();const parent=preset.from?artifactBySelector(preset.from):null;form.elements.from.value=parent?parent.id:'';form.elements.enable_git.checked=Boolean(preset.git);form.elements.enable_acp.checked=Boolean(preset.acp);form.elements.command.value=preset.command||'';syncRecordForm();}
function syncRecordForm(){const form=document.getElementById('record-form');document.getElementById('record-command-field').hidden=!form.elements.enable_acp.checked;}
function recordFromOptions(){const seen=new Set();return snapshot.artifacts.filter(artifact=>canUse(artifact)&&!artifact.superseded_by).map(artifact=>({id:artifact.id,label:artifactSelector(artifact)+(artifact.scope==='user'?' · user':'')})).filter(option=>{if(seen.has(option.label))return false;seen.add(option.label);return true;}).sort((a,b)=>a.label.localeCompare(b.label));}
function renderRecordFromOptions(){const select=document.getElementById('record-from'),current=select.value,options=recordFromOptions();select.innerHTML='<option value="">Alpine-basis</option>'+options.map(option=>`<option value="${esc(option.id)}">${esc(option.label)}</option>`).join('');if(options.some(option=>option.id===current))select.value=current;}
function artifactBySelector(selector){return snapshot.artifacts.filter(artifact=>canUse(artifact)&&!artifact.superseded_by&&artifactSelector(artifact)===selector).sort((a,b)=>(a.scope==='user'?0:1)-(b.scope==='user'?0:1))[0]||null;}
function recordRequestFromForm(){const form=document.getElementById('record-form'),enables=[];if(form.elements.enable_git.checked)enables.push({name:'git'});if(form.elements.enable_acp.checked)enables.push({name:'acp',command:form.elements.command.value.trim()||undefined});return {kind:form.elements.kind.value,name:form.elements.name.value.trim().toLowerCase(),scope:form.elements.scope.value,parent_artifact_ids:form.elements.from.value?[form.elements.from.value]:[],enables};}
function openConsole(){
  openDialog('capsule-dialog');
  requestAnimationFrame(()=>{ensureShell();focusTerminal();});
}
// openLayerDialog asks what the new layer is; the recording starts from it.
function openLayerDialog(preset={}){if(activeRecording()){showError(new Error('Je hebt al een opname open; sluit die eerst af met End & save of Cancel.'));openConsole();return;}fillRecordForm(preset);openDialog('layer-dialog');requestAnimationFrame(()=>document.getElementById('record-form').elements.name.focus());}
function syntaxLanguage(hint=''){
  const aliases={js:'javascript',mjs:'javascript',cjs:'javascript',jsx:'javascript',javascript:'javascript',ts:'typescript',tsx:'typescript',typescript:'typescript',go:'go',cs:'csharp','c#':'csharp',csharp:'csharp',java:'java',c:'c',h:'c',cc:'cpp',cpp:'cpp',cxx:'cpp',hpp:'cpp',rs:'rust',rust:'rust',swift:'swift',kt:'kotlin',kts:'kotlin',kotlin:'kotlin',php:'php',py:'python',python:'python',rb:'ruby',ruby:'ruby',sh:'shell',bash:'shell',zsh:'shell',shell:'shell',json:'json',jsonc:'json',yaml:'yaml',yml:'yaml',toml:'toml',html:'markup',htm:'markup',xml:'markup',svg:'markup',vue:'markup',svelte:'markup',razor:'markup',cshtml:'markup',markup:'markup',css:'css',scss:'css',sass:'css',less:'css',sql:'sql',md:'markdown',markdown:'markdown',mmd:'mermaid',mermaid:'mermaid',dockerfile:'docker',docker:'docker'};
  const raw=String(hint||'').trim().toLowerCase().split(/\s+/)[0].split(/[?#]/)[0],base=raw.split('/').pop()||raw;if(aliases[raw])return aliases[raw];if(aliases[base])return aliases[base];const extension=base.includes('.')?base.split('.').pop():'';return aliases[extension]||'';
}
function syntaxMatcher(language){
  if(language==='markup')return /(?<comment><!--[\s\S]*?-->)|(?<tag><\/?[A-Za-z][^>]*>)|(?<literal>&(?:#\d+|#x[\da-f]+|[a-z]+);)/gi;
  if(language==='css')return /(?<comment>\/\*[\s\S]*?\*\/|\/\/[^\n]*)|(?<string>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')|(?<meta>@[\w-]+)|(?<property>--?[\w-]+(?=\s*:)|[\w-]+(?=\s*:))|(?<number>\b(?:0x[\da-f]+|\d+(?:\.\d+)?)(?:%|px|r?em|vh|vw|s|ms|deg)?\b)|(?<literal>#[\da-f]{3,8}\b|\b(?:inherit|initial|unset|transparent|currentColor)\b)/gi;
  if(language==='json')return /(?<string>"(?:\\.|[^"\\])*")|(?<number>-?\b(?:0|[1-9]\d*)(?:\.\d+)?(?:e[+-]?\d+)?\b)|(?<literal>\b(?:true|false|null)\b)/gi;
  if(language==='yaml'||language==='toml')return /(?<comment>#[^\n]*)|(?<string>"(?:\\.|[^"\\])*"|'(?:''|[^'])*')|(?<property>^[ \t-]*[A-Za-z_][\w.-]*(?=\s*[:=]))|(?<number>-?\b(?:0x[\da-f]+|\d+(?:\.\d+)?)\b)|(?<literal>\b(?:true|false|null|yes|no|on|off)\b)|(?<meta>^\s*\[[^\]\n]+\])/gim;
  if(language==='shell'||language==='docker')return /(?<comment>#[^\n]*)|(?<string>"(?:\\.|[^"\\])*"|'[^']*')|(?<variable>\$(?:\{[^}]+\}|[A-Za-z_]\w*|\d+|[?#@*!-]))|(?<keyword>\b(?:if|then|else|elif|fi|for|while|until|do|done|case|esac|in|function|select|time|coproc|FROM|RUN|COPY|ADD|ARG|ENV|WORKDIR|ENTRYPOINT|CMD|EXPOSE|VOLUME|USER|LABEL|HEALTHCHECK)\b)|(?<number>\b\d+(?:\.\d+)?\b)|(?<literal>\b(?:true|false|null)\b)/g;
  if(language==='python'||language==='ruby')return /(?<comment>#[^\n]*)|(?<string>'''[\s\S]*?'''|"""[\s\S]*?"""|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')|(?<meta>@[A-Za-z_]\w*(?:\.\w+)*)|(?<keyword>\b(?:and|as|assert|async|await|begin|break|case|class|def|defined|del|do|elif|else|elsif|end|ensure|except|for|from|global|if|import|in|is|lambda|module|next|nonlocal|not|or|pass|raise|redo|rescue|retry|return|self|super|then|unless|until|when|while|with|yield)\b)|(?<number>\b(?:0x[\da-f]+|\d+(?:\.\d+)?)\b)|(?<literal>\b(?:True|False|None|true|false|nil)\b)/gi;
  if(language==='sql')return /(?<comment>--[^\n]*|\/\*[\s\S]*?\*\/)|(?<string>'(?:''|[^'])*'|"(?:""|[^"])*")|(?<keyword>\b(?:add|alter|and|as|asc|begin|between|by|case|commit|constraint|create|cross|database|default|delete|desc|distinct|drop|else|end|exists|foreign|from|full|group|having|in|index|inner|insert|into|is|join|key|left|like|limit|not|null|on|or|order|outer|primary|references|returning|right|rollback|select|set|table|then|union|unique|update|values|view|when|where|with)\b)|(?<number>\b\d+(?:\.\d+)?\b)|(?<literal>\b(?:true|false|null)\b)/gi;
  if(language==='markdown')return /(?<meta>^#{1,6}\s+[^\n]+|^>\s+|^[-*+]\s+|^\d+[.)]\s+)|(?<string>`[^`]+`)|(?<keyword>\*\*[^*]+\*\*|__[^_]+__)|(?<tag>\[[^\]]+\]\([^)]+\))/gm;
  return /(?<comment>\/\/[^\n]*|\/\*[\s\S]*?\*\/)|(?<string>`(?:\\[\s\S]|[^`\\])*`|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')|(?<meta>^\s*#[A-Za-z_]+[^\n]*|@[A-Za-z_]\w*)|(?<variable>\$[A-Za-z_]\w*)|(?<keyword>\b(?:abstract|as|async|await|base|break|case|catch|chan|class|const|continue|default|defer|do|else|enum|export|extends|extern|fallthrough|final|finally|fn|for|foreach|from|func|function|go|goto|if|implements|import|in|interface|internal|is|let|lock|match|namespace|new|of|out|override|package|private|protected|public|readonly|ref|return|select|static|struct|super|switch|this|throw|trait|try|type|typeof|unsafe|use|using|var|virtual|where|while|yield)\b)|(?<type>\b(?:any|bool|boolean|byte|char|decimal|double|dynamic|error|float|i8|i16|i32|i64|int|long|map|never|object|rune|short|string|u8|u16|u32|u64|uint|ulong|unknown|usize|void)\b)|(?<number>\b(?:0x[\da-f]+|0b[01]+|\d+(?:\.\d+)?(?:e[+-]?\d+)?)\b)|(?<literal>\b(?:true|false|null|nil|undefined|None)\b)/gim;
}
function highlightCode(value,hint=''){
  const source=String(value??''),language=syntaxLanguage(hint);if(!language)return esc(source);const matcher=syntaxMatcher(language);let output='',offset=0,match;
  while((match=matcher.exec(source))){output+=esc(source.slice(offset,match.index));let kind=Object.keys(match.groups||{}).find(name=>match.groups[name]!==undefined)||'';if(language==='json'&&kind==='string'&&/^\s*:/.test(source.slice(matcher.lastIndex)))kind='property';output+=`<span class="syn-${kind}">${esc(match[0])}</span>`;offset=matcher.lastIndex;if(match[0]==='')matcher.lastIndex++;}
  return output+esc(source.slice(offset));
}
function sourceCode(value,hint='',className='syntax-code'){
  const language=syntaxLanguage(hint);return `<pre class="${esc(className)}"><code data-language="${esc(language||'plain')}">${highlightCode(value,language)}</code></pre>`;
}
const markdownRenderer=new marked.Renderer(),defaultTableRenderer=markdownRenderer.table,defaultLinkRenderer=markdownRenderer.link;
markdownRenderer.code=function({text,lang}){const language=syntaxLanguage(String(lang||'').split(/\s+/)[0]);if(language==='mermaid')return `<div class="mermaid-shell"><div class="mermaid mermaid-loading" data-mermaid-pending="true">${esc(text)}</div></div>`;return sourceCode(text,language||lang||'');};
markdownRenderer.table=function(token){return `<div class="md-table-scroll">${defaultTableRenderer.call(this,token)}</div>`;};
markdownRenderer.link=function(token){return defaultLinkRenderer.call(this,token).replace(/^<a /,'<a target="_blank" rel="noopener noreferrer" ');};
function markdown(value){
  const source=String(value||'').replace(/^[\u200B\u200C\u200D\u200E\u200F\uFEFF]/,'');try{return DOMPurify.sanitize(marked.parse(source,{gfm:true,breaks:true,renderer:markdownRenderer}),{USE_PROFILES:{html:true},ADD_ATTR:['target','rel'],FORBID_TAGS:['style','form','button','textarea','select','option'],FORBID_ATTR:['style'],SANITIZE_NAMED_PROPS:true});}catch(error){return `<pre class="syntax-code markdown-error"><code>${esc(source)}</code></pre>`;}
}
let mermaidLoader=null;
function loadMermaid(){
  if(window.mermaid)return Promise.resolve(window.mermaid);if(mermaidLoader)return mermaidLoader;mermaidLoader=new Promise((resolve,reject)=>{const script=document.createElement('script');script.src=`${spinAssetBase}/vendor/mermaid-11.17.2.min.js`;script.onload=()=>{if(!window.mermaid){reject(new Error('Mermaid library ontbreekt'));return;}window.mermaid.initialize({startOnLoad:false,securityLevel:'strict',theme:'dark',suppressErrorRendering:true,themeVariables:{background:'#101419',primaryColor:'#1a2735',primaryTextColor:'#e9edf1',primaryBorderColor:'#46525f',lineColor:'#82a8d2',secondaryColor:'#202730',tertiaryColor:'#2a2217',fontFamily:'Inter, ui-sans-serif, system-ui, sans-serif'}});resolve(window.mermaid);};script.onerror=()=>reject(new Error('Mermaid kon niet worden geladen'));document.head.appendChild(script);});return mermaidLoader;
}
async function renderMermaid(root){
  const nodes=[...(root?.matches?.('.mermaid[data-mermaid-pending]')?[root]:[]),...(root?.querySelectorAll?.('.mermaid[data-mermaid-pending]')||[])];if(!nodes.length)return;nodes.forEach(node=>node.dataset.mermaidPending='rendering');try{const engine=await loadMermaid();for(const node of nodes){if(!node.isConnected)continue;const source=node.textContent||'';try{await engine.run({nodes:[node],suppressErrors:true});if(!node.querySelector('svg'))throw new Error('ongeldige Mermaid-syntax');node.classList.remove('mermaid-loading');node.removeAttribute('data-mermaid-pending');}catch(error){node.className='mermaid-error';node.removeAttribute('data-mermaid-pending');node.textContent=`Diagram kon niet worden gerenderd: ${error.message||error}\n\n${source}`;}}}catch(error){nodes.forEach(node=>{if(!node.isConnected)return;node.className='mermaid-error';node.removeAttribute('data-mermaid-pending');node.textContent=error.message||String(error);});}
}
function setMarkdown(root,value){root.innerHTML=markdown(value);return renderMermaid(root);}
// The server replays the whole conversation on every connect, including the
// reconnect after a dropped socket. The feed is rebuilt from that replay, so it
// is emptied first; appending onto what was already there showed every message
// a second time.
function resetChatFeed(){
  chatState.messageNodes.clear();chatState.toolNodes.clear();chatState.thoughtNodes.clear();chatState.metaNodes.clear();chatState.planNode=null;chatState.followTail=true;
  const usage=document.getElementById('chat-usage');usage.hidden=true;usage.textContent='';
  document.getElementById('chat-feed-inner').innerHTML='<div class="chat-welcome"><strong>ACP Session</strong>De verbinding en agent-context worden opgebouwd…</div>';
}
function resetChatView(){
  resetChatFeed();chatState.busy=false;chatState.waitingShown=false;closeDiff();
  renderChatBusy();renderChanges({branch:'workspace laden…',added:0,deleted:0,files:[]});
}
function chatAtTail(feed=document.getElementById('chat-feed')){return feed.scrollHeight-feed.scrollTop-feed.clientHeight<=8;}
function followChatTail(){if(!chatState.followTail)return;requestAnimationFrame(()=>{if(!chatState.followTail)return;const feed=document.getElementById('chat-feed');feed.scrollTop=feed.scrollHeight;});}
function chatAppend(node){const root=document.getElementById('chat-feed-inner');root.querySelector('.chat-welcome')?.remove();root.appendChild(node);followChatTail();return node;}
function chatSystem(text,error=false){const node=document.createElement('div');node.className=error?'chat-system line error':'chat-system';node.textContent=text;chatAppend(node);}
// chatEventAt is the time of the event being rendered: the server stamps
// every event, and a replayed history keeps its original times, so the feed
// shows when things happened rather than when the browser drew them.
let chatEventAt=null;
function chatClock(at){const date=at?new Date(at):new Date();return date.toLocaleTimeString([], {hour:'2-digit',minute:'2-digit',second:'2-digit'});}
function chatMessage(role,text,key=''){
  const node=document.createElement('article');node.className=`chat-message ${role}`;node.innerHTML=`<div class="chat-role">${role==='user'?'You':'Agent'}<time class="chat-time" title="${esc(chatEventAt||'')}">${esc(chatClock(chatEventAt))}</time></div><div class="chat-bubble"><div class="md"></div></div>`;const body=node.querySelector('.md');body.dataset.source=text||'';setMarkdown(body,text);chatAppend(node);if(key)chatState.messageNodes.set(key,node);return node;
}
function formatToolValue(value){
  let output='';try{output=typeof value==='string'?value:JSON.stringify(value,null,2);}catch(_){output=String(value??'');}
  return output.length>200000?`${output.slice(0,200000)}\n… output truncated in browser`:output;
}
function renderACPContent(content,hint='',asSource=false){
  if(!content||typeof content!=='object')return `<pre>${esc(formatToolValue(content))}</pre>`;
  if(content.type==='text')return asSource?sourceCode(content.text||'',hint,'tool-code syntax-code'):`<div class="md">${markdown(content.text||'')}</div>`;
  if(content.type==='image'&&/^image\//.test(content.mimeType||'')&&content.data)return `<img class="acp-image" alt="ACP image" src="data:${esc(content.mimeType)};base64,${esc(content.data)}">`;
  if(content.type==='audio'&&/^audio\//.test(content.mimeType||'')&&content.data)return `<audio class="acp-audio" controls preload="none" src="data:${esc(content.mimeType)};base64,${esc(content.data)}"></audio>`;
  if(content.type==='audio')return `<div class="acp-attachment">Audio · ${esc(content.mimeType||'unknown format')}</div>`;
  if(content.type==='resource_link')return `<div class="acp-resource"><strong>${esc(content.name||content.title||'Resource')}</strong><small>${esc(content.uri||'')}</small>${content.description?`<p>${esc(content.description)}</p>`:''}</div>`;
  if(content.type==='resource'&&content.resource){const resource=content.resource;return `<div class="acp-resource"><strong>${esc(resource.uri||'Resource')}</strong>${resource.text!=null?sourceCode(resource.text,resource.uri||hint,'tool-code syntax-code'):`<small>${esc(resource.mimeType||'binary resource')}</small>`}</div>`;}
  return `<pre>${esc(formatToolValue(content))}</pre>`;
}
function appendContent(role,update){
  const content=update.content||{};
  if(content.type!=='text'){
    const node=document.createElement('section');node.className='chat-event content-card';node.innerHTML=`<div class="chat-event-body">${renderACPContent(content)}</div>`;chatAppend(node);renderMermaid(node);return;
  }
  const text=String(content.text||'');if(!text)return;
  const base=role==='agent'?'agent':'user',key=`${base}:${update.messageId||'current'}`;let node=chatState.messageNodes.get(key);if(!node)node=chatMessage(base,'',key);
  const body=node.querySelector('.md');body.dataset.source=(body.dataset.source||'')+text;setMarkdown(body,body.dataset.source);
  followChatTail();
}
function appendThought(update){
  const content=update.content||{},text=content.type==='text'?String(content.text||''):'';if(!text)return;const key=`thought:${update.messageId||'current'}`;let node=chatState.thoughtNodes.get(key);
  if(!node){node=document.createElement('details');node.className='chat-event chat-thought';node.innerHTML='<summary>Thinking</summary><div class="chat-event-body md"></div>';chatAppend(node);chatState.thoughtNodes.set(key,node);}
  const body=node.querySelector('.chat-event-body');body.dataset.source=(body.dataset.source||'')+text;setMarkdown(body,body.dataset.source);
}
function toolIcon(kind){return ({execute:'terminal',read:'description',edit:'edit_note',delete:'delete',move:'drive_file_move',search:'search',think:'psychology',fetch:'download',switch_mode:'swap_horiz'})[kind]||'build';}
function toolStateIcon(status){return ({completed:'check',failed:'error',pending:'more_horiz',in_progress:'more_horiz'})[status]||'more_horiz';}
function toolCommand(tool){
  const input=tool.rawInput;if(typeof input==='string')return input;if(!input||typeof input!=='object')return '';
  const command=input.command??input.cmd??input.script;if(Array.isArray(command))return command.join(' ');if(typeof command==='string')return command;
  return '';
}
function commandResult(value){
  if(typeof value==='string')return {text:value,meta:'',failed:false};if(!value||typeof value!=='object')return {text:formatToolValue(value),meta:'',failed:false};
  const output=[],formatted=value.formatted_output??value.formattedOutput;if(typeof formatted==='string'&&formatted)output.push(formatted);else ['output','stdout','stderr'].forEach(key=>{if(typeof value[key]==='string'&&value[key]&&!output.includes(value[key]))output.push(value[key]);});const metadata=value.metadata&&typeof value.metadata==='object'?value.metadata:{};
  const exit=value.exitCode??value.exit_code??metadata.exitCode??metadata.exit_code,duration=value.duration??value.durationSeconds??metadata.duration_seconds;const meta=[];if(exit!=null)meta.push(`exit ${exit}`);if(duration!=null)meta.push(typeof duration==='number'?`${duration}s`:String(duration));
  return {text:output.length?output.join('\n'):formatToolValue(value),meta:meta.join(' · '),failed:Number(exit)>0};
}
// shortPath keeps the file name whole and elides the middle of the
// directories, so a long path still tells you which file it is.
// A new file is green, a removed one red, the rest amber.
function changeStatusClass(status){const letter=String(status||'M').trim().charAt(0).toUpperCase();return letter==='A'||letter==='?'?'added':letter==='D'?'deleted':'';}
function shortPath(path,max=44){path=String(path||'');if(path.length<=max)return path;const parts=path.split('/'),name=parts.pop(),head=parts.length?parts[0]+'/':'';const room=Math.max(0,max-name.length-head.length-2);if(room<=0||parts.length<=1)return `${head}…/${name}`;let tail='';for(let index=parts.length-1;index>0;index--){const candidate=parts[index]+'/'+tail;if(candidate.length>room)break;tail=candidate;}return `${head}…/${tail}${name}`;}
function workspacePath(path){return String(path||'').replace(/^\/workspace\//,'').replace(/^\.\//,'');}
function toolPath(tool,item={}){const input=tool.rawInput&&typeof tool.rawInput==='object'?tool.rawInput:{};return workspacePath(item.path||input.path||input.filePath||input.file_path||input.filename||tool.locations?.[0]?.path||'');}
function textLines(value){const text=String(value??'');return text===''?[]:text.replace(/\n$/,'').split('\n');}
function toolDiffFile(item,tool){
  const before=textLines(item.oldText),after=textLines(item.newText);let prefix=0;while(prefix<before.length&&prefix<after.length&&before[prefix]===after[prefix])prefix++;let suffix=0;while(suffix<before.length-prefix&&suffix<after.length-prefix&&before[before.length-1-suffix]===after[after.length-1-suffix])suffix++;
  const leading=Math.min(3,prefix),trailing=Math.min(3,suffix),oldStart=prefix-leading,newStart=prefix-leading,oldChangedEnd=before.length-suffix,newChangedEnd=after.length-suffix,patch=[];before.slice(oldStart,prefix).forEach(line=>patch.push(` ${line}`));before.slice(prefix,oldChangedEnd).forEach(line=>patch.push(`-${line}`));after.slice(prefix,newChangedEnd).forEach(line=>patch.push(`+${line}`));before.slice(oldChangedEnd,oldChangedEnd+trailing).forEach(line=>patch.push(` ${line}`));const oldCount=leading+(oldChangedEnd-prefix)+trailing,newCount=leading+(newChangedEnd-prefix)+trailing;
  return {path:toolPath(tool,item),patch:`@@ -${oldCount?oldStart+1:0},${oldCount} +${newCount?newStart+1:0},${newCount} @@\n${patch.join('\n')}`,added:newChangedEnd-prefix,deleted:oldChangedEnd-prefix};
}
function toolSection(label,body,className=''){return `<section class="tool-section ${className}"><div class="tool-label">${esc(label)}</div>${body}</section>`;}
function renderToolBody(tool){
  const sections=[],contentTexts=[],command=toolCommand(tool),path=toolPath(tool);
  if(command)sections.push(toolSection('Command',`<pre class="tool-code">${esc(command)}</pre>`));
  else if(tool.rawInput!=null)sections.push(toolSection('Input',`<pre class="tool-code">${esc(formatToolValue(tool.rawInput))}</pre>`));
  (Array.isArray(tool.content)?tool.content:[]).forEach(item=>{
    if(item?.type==='content'){if(item.content?.type==='text')contentTexts.push(String(item.content.text||''));sections.push(toolSection(tool.kind==='read'&&path?`Read · ${shortPath(path,60)}`:'Result',renderACPContent(item.content,path,tool.kind==='read'),tool.kind==='execute'?'tool-output':''));}
    else if(item?.type==='diff'){
      const file=toolDiffFile(item,tool),changePath=file.path;
      sections.push(toolSection('Change',`<button class="tool-diff-link" type="button" data-open-change="${esc(changePath)}" title="${esc(changePath)}"><span>${esc(shortPath(changePath||'file',56))}</span><span class="diff-add">+${file.added}</span><span class="diff-del">−${file.deleted}</span></button><div class="tool-change-preview">${renderDiff(file)}</div><div class="tool-diff-note">Klik op het bestand voor de volledige Git-diff.</div>`));
    } else if(item?.type==='terminal')sections.push(toolSection('Terminal',`<pre class="tool-code tool-terminal">${esc(item.terminalId||'attached')}</pre>`));
  });
  if(tool.rawOutput!=null){const result=commandResult(tool.rawOutput);if(!contentTexts.includes(result.text)){const body=tool.kind==='read'&&path&&typeof tool.rawOutput==='string'?sourceCode(result.text,path,'tool-code tool-output syntax-code'):`<pre class="tool-code tool-output">${esc(result.text)}</pre>`;sections.push(toolSection(result.meta?`Output · ${result.meta}`:'Output',body,result.failed?'tool-output error':''));}}
  if(Array.isArray(tool.locations)&&tool.locations.length)sections.push(toolSection('Locations',`<div class="tool-locations">${tool.locations.map(location=>`<button type="button" data-open-change="${esc(workspacePath(location.path))}">${esc(workspacePath(location.path))}${location.line?`:${esc(location.line)}`:''}</button>`).join('')}</div>`));
  return sections.join('')||'<div class="tool-empty">Waiting for output…</div>';
}
function upsertTool(update){
  const id=update.toolCallId||`tool-${chatState.toolNodes.size}`,existing=chatState.toolNodes.get(id);let node=existing?.node,tool={...(existing?.tool||{}),...update};
  if(!node){node=document.createElement('details');node.className='chat-event tool-card';chatAppend(node);}
  const wasOpen=node.open,status=tool.status||'pending',kind=tool.kind||'other',title=tool.title||kind||'Tool call',command=toolCommand(tool),subtitle=command&&command!==title?command:'';
  const startedAt=existing?.startedAt||chatEventAt,updatedAt=chatEventAt;
  node.className=`chat-event tool-card ${esc(kind)} ${esc(status)}`;node.innerHTML=`<summary><span class="tool-icon material-symbols-outlined" title="${esc(kind)}" aria-label="${esc(kind)}">${esc(toolIcon(kind))}</span><span class="tool-summary"><strong class="tool-title">${esc(title)}</strong>${subtitle?`<small class="tool-subtitle">${esc(subtitle)}</small>`:''}</span><span class="tool-state ${esc(status)}" title="${esc(status.replaceAll('_',' '))}"><span class="material-symbols-outlined" aria-hidden="true">${esc(toolStateIcon(status))}</span>${status==='failed'?'<span class="tool-state-label">mislukt</span>':''}</span><time class="chat-time" title="gestart ${esc(chatClock(startedAt))}${updatedAt&&updatedAt!==startedAt?` · laatste update ${esc(chatClock(updatedAt))}`:''}">${esc(chatClock(updatedAt||startedAt))}</time></summary><div class="tool-body">${renderToolBody(tool)}</div>`;
  node.open=status==='failed'||wasOpen;node.querySelectorAll('[data-open-change]').forEach(button=>button.onclick=()=>openDiff(button.dataset.openChange));renderMermaid(node);chatState.toolNodes.set(id,{node,tool,startedAt});if(status==='completed'||status==='failed')scheduleChatChanges();
}
function renderPlan(update){
  if(!chatState.planNode){chatState.planNode=document.createElement('section');chatState.planNode.className='chat-event';chatAppend(chatState.planNode);}
  chatState.planNode.innerHTML=`<div class="chat-plan">${(update.entries||[]).map(entry=>`<div class="plan-row"><span>${entry.status==='completed'?'✓':entry.status==='in_progress'?'●':'○'}</span><span>${esc(entry.content||'')}</span><span class="tag">${esc(entry.status||entry.priority||'pending')}</span></div>`).join('')}</div>`;
}
function renderAutoAccept(enabled){const button=document.getElementById('chat-auto-accept');if(!button)return;chatState.autoAccept=enabled;button.classList.toggle('active',enabled);button.title=enabled?'Toestemmingsvragen worden automatisch toegestaan; klik om zelf te beslissen':'Toestemmingsvragen worden aan jou voorgelegd; klik om automatisch toe te staan';document.getElementById('chat-auto-accept-label').textContent=enabled?'Auto':'Vragen';}
function renderPermission(event){
  const params=event.params||{},tool=params.toolCall||{},node=document.createElement('section');
  if(event.auto){node.className='chat-event permission-card auto';node.innerHTML=`<div class="chat-event-body"><span class="tool-state completed"><span class="material-symbols-outlined" aria-hidden="true">check</span></span> Automatisch toegestaan · ${esc(tool.title||'toestemming')}${event.choice?` · ${esc(event.choice)}`:''}</div>`;chatAppend(node);return;}
  node.className='chat-event permission-card';node.innerHTML=`<div class="chat-event-body"><strong>${esc(tool.title||'Permission required')}</strong><div class="permission-actions">${(params.options||[]).map(option=>`<button class="${String(option.kind||'').startsWith('reject')?'danger':'small-button'}" data-permission-option="${esc(option.optionId)}">${esc(option.name)}</button>`).join('')}</div></div>`;node.querySelectorAll('[data-permission-option]').forEach(button=>button.onclick=()=>{sendChat({type:'permission',request_id:event.request_id,option_id:button.dataset.permissionOption});node.remove();});chatAppend(node);
}
function upsertMeta(key,title,value){
  let node=chatState.metaNodes.get(key);if(!node){node=document.createElement('details');node.className='chat-event acp-meta-card';chatAppend(node);chatState.metaNodes.set(key,node);}
  node.innerHTML=`<summary><span>${esc(title)}</span><span class="tag">ACP</span></summary><div class="chat-event-body"><pre class="tool-code">${esc(formatToolValue(value))}</pre></div>`;
}
function renderUsage(update){
  const used=update.used??update.tokensUsed,size=update.size??update.contextWindow,cost=update.cost;const parts=[];
  const compact=value=>{const n=Number(value);return n>=1e6?`${(n/1e6).toFixed(1)}M`:n>=1e3?`${Math.round(n/1e3)}k`:String(n);};
  if(used!=null&&size!=null)parts.push(`${compact(used)} / ${compact(size)}`);else if(used!=null)parts.push(`${compact(used)} tokens`);
  if(cost?.amount!=null)parts.push(`${cost.amount} ${cost.currency||''}`.trim());else if(typeof cost==='number')parts.push(String(cost));
  const badge=document.getElementById('chat-usage');badge.textContent=parts.join(' · ');badge.title=used!=null&&size!=null?`${Number(used).toLocaleString()} van ${Number(size).toLocaleString()} tokens gebruikt`:'';badge.hidden=!parts.length;
}
function handleACPEvent(event){
  chatEventAt=event.at||new Date().toISOString();if(event.type!=='ready')chatState.lastActivity=chatEventAt;
  if(event.type==='ready'){resetChatFeed();renderChatQuestion(chatState.sessionID);chatState.busy=!!event.busy;chatState.queued=event.queued||0;chatState.viewer=!!event.viewer;document.getElementById('chat-composer').hidden=!!event.viewer;document.getElementById('chat-viewer').hidden=!event.viewer;document.getElementById('chat-viewer-text').textContent=`Je kijkt mee met ${event.operator||'de werker'}; alleen die stuurt hier`;document.getElementById('chat-auto-accept').hidden=!!event.viewer;document.getElementById('chat-status').innerHTML=`<span class="dot"></span><span>${esc(event.agent_name||'ACP')} · <span class="chat-live-label">ready</span></span>`;document.getElementById('chat-context').dataset.agentSession=event.agent_session_id||'';renderChatBusy();scheduleChatChanges(true);return;}
  if(event.type==='user'){chatMessage('user',event.text||'');chatState.messageNodes.delete('agent:current');chatState.thoughtNodes.delete('thought:current');chatState.busy=true;chatState.queued=event.queued||0;renderChatBusy();return;}
  if(event.type==='update'){const update=event.update||{};switch(update.sessionUpdate){case'user_message_chunk':appendContent('user',update);break;case'agent_message_chunk':appendContent('agent',update);break;case'agent_thought_chunk':appendThought(update);break;case'tool_call':case'tool_call_update':upsertTool(update);break;case'plan':renderPlan(update);break;case'current_mode_update':upsertMeta('mode','Mode',update.currentModeId||update.modeId||update);break;case'available_commands_update':upsertMeta('commands','Available commands',update.availableCommands||update.commands||update);break;case'config_option_update':upsertMeta('config','Session options',update.configOptions||update.options||update);break;case'session_info_update':upsertMeta('session-info','Session info',update);break;case'usage_update':renderUsage(update);break;default:upsertMeta(`update:${update.sessionUpdate||'unknown'}`,update.sessionUpdate||'ACP update',update);}return;}
  if(event.type==='permission'){renderPermission(event);return;}
  if(event.type==='auto_accept'){renderAutoAccept(event.enabled!==false);return;}
  if(event.type==='queued'){chatState.queued=event.queued||0;if(event.text)chatSystem(event.queued?`In wachtrij: ${event.text}`:event.text);renderChatBusy();return;}
  if(event.type==='steered'){chatSystem(event.text||'Ingestuurd in de lopende beurt');return;}
  if(event.type==='turn_end'){chatState.queued=event.queued||0;chatState.busy=chatState.queued>0;renderChatBusy();if(!chatState.busy)setTimeout(async()=>{await refresh(false);rerenderChatQuestion();},400);chatSystem(event.stop_reason?`Turn ended · ${event.stop_reason}`:'Turn ended');chatState.messageNodes.delete('agent:current');chatState.thoughtNodes.delete('thought:current');chatState.planNode=null;scheduleChatChanges(true);return;}
  if(event.type==='error'){chatState.busy=false;chatState.queued=0;if(event.fatal)chatState.manualClose=true;renderChatBusy();chatSystem(event.error||'ACP error',true);}
}
function renderChatBusy(){const ready=chatState.socket?.readyState===WebSocket.OPEN;document.getElementById('chat-send').disabled=!ready;document.getElementById('chat-cancel').hidden=!chatState.busy;document.getElementById('chat-input').disabled=!ready;
  document.querySelectorAll('[data-chat-question]').forEach(card=>{card.querySelectorAll('.panel-actions button').forEach(button=>button.disabled=chatState.busy);const hint=card.querySelector('[data-question-busy]');if(hint)hint.hidden=!chatState.busy;});
  const status=document.getElementById('chat-status'),label=status?.querySelector('.chat-live-label');if(status)status.className=`chat-live ${chatState.busy?'busy':'ready'}`;
  // While the agent works, say how long ago it last did anything: a turn
  // can be silent for minutes, and silence for much longer is worth seeing.
  const activity=()=>{if(!label)return;if(!chatState.busy){label.textContent='ready';label.title='';return;}const since=chatState.lastActivity?Math.max(0,Math.round((Date.now()-new Date(chatState.lastActivity).getTime())/1000)):null,short=since==null?'':since<60?`${since}s`:`${Math.floor(since/60)}m${String(since%60).padStart(2,'0')}s`;label.textContent=`${chatState.queued?`working +${chatState.queued}`:'working'}${short?` · ${short}`:''}`;label.title=`${chatState.queued?`${chatState.queued} bericht(en) in wachtrij · `:''}laatste activiteit ${short||'onbekend'} geleden`;label.classList.toggle('stale',since!=null&&since>180);};
  activity();clearInterval(chatState.activityTimer);if(chatState.busy)chatState.activityTimer=setInterval(activity,1000);
  document.getElementById('chat-input').placeholder='Stuur een bericht';}
function sendChat(message){if(chatState.socket?.readyState===WebSocket.OPEN)chatState.socket.send(JSON.stringify(message));}
// The capsule of a fresh Session is still on its way; the chat shows where
// it stands and connects the moment the agent can be reached.
function chatSessionReady(sessionID){const session=byID(snapshot.sessions,sessionID),composition=byID(snapshot.compositions,session?.prepared_composition_id);return Boolean(composition?.runtime&&composition.runtime.status!=='stopped');}
function chatWaitingLabel(session){const state=preparationText(sessionPreparation(session),session);return state.failed?state.text:`Even geduld · ${state.text}`;}
function connectACPChat(sessionID){
  chatState.manualClose=false;
  if(!chatSessionReady(sessionID)){const session=byID(snapshot.sessions,sessionID),status=document.getElementById('chat-status');status.className='chat-live busy';status.innerHTML=`<span class="dot"></span><span>${esc(chatWaitingLabel(session))}</span>`;if(!chatState.waitingShown){chatState.waitingShown=true;chatSystem('De omgeving van deze Session wordt klaargezet. Zodra de agent er is, kun je typen.');}clearTimeout(chatState.reconnectTimer);chatState.reconnectTimer=setTimeout(()=>{if(document.getElementById('chat-dialog').open&&chatState.sessionID===sessionID)connectACPChat(sessionID);},1500);return;}
  const protocol=location.protocol==='https:'?'wss:':'ws:',socket=new WebSocket(`${protocol}//${location.host}/api/sessions/${encodeURIComponent(sessionID)}/acp`);chatState.socket=socket;
  socket.onopen=()=>{document.getElementById('chat-status').className='chat-live busy';document.getElementById('chat-status').innerHTML='<span class="dot"></span><span>ACP session starten…</span>';renderChatBusy();};
  socket.onmessage=message=>{try{handleACPEvent(JSON.parse(message.data));}catch(error){chatSystem(`Invalid ACP event: ${error.message||error}`,true);}};
  socket.onerror=()=>{document.getElementById('chat-status').className='chat-live';document.getElementById('chat-status').innerHTML='<span class="dot"></span><span>connection error</span>';};
  socket.onclose=()=>{if(chatState.socket!==socket)return;chatState.busy=false;renderChatBusy();document.getElementById('chat-status').className='chat-live';document.getElementById('chat-status').innerHTML='<span class="dot"></span><span>disconnected</span>';if(!chatState.manualClose&&document.getElementById('chat-dialog').open){clearTimeout(chatState.reconnectTimer);chatState.reconnectTimer=setTimeout(()=>connectACPChat(sessionID),1500);}};
}
function workflowSessionIsActive(sessionID){const session=byID(snapshot.sessions,sessionID);if(!session?.phase_run_id)return !!session;const job=byID(snapshot.jobs,session.job_id),run=byID(snapshot.phase_runs,session.phase_run_id);return job?.current_phase_run_id===session.phase_run_id&&['queued','running','pending'].includes(run?.status);}
function openACPChat(sessionID){
  const session=byID(snapshot.sessions,sessionID),job=byID(snapshot.jobs,session?.job_id),repository=byID(snapshot.git_repositories,session?.git_repository_id);if(!session)return;if(!workflowSessionIsActive(sessionID)){showError(new Error('Alleen de actieve workflowstap heeft een live Chat.'));return;}
  chatState.manualClose=true;if(chatState.socket)chatState.socket.close();clearTimeout(chatState.reconnectTimer);chatState.sessionID=sessionID;resetChatView();
  document.getElementById('chat-title').textContent=job?.title||'Session';document.getElementById('chat-role').textContent=session.role||'worker';document.getElementById('chat-context').textContent=`${repository?.name||'Git'} · ${session.git_ref} · ${session.environment_selector||'tool:'+session.tool}`;
  // The same Retry as on the Job card, for a step that went wrong while the
  // chat is open (an agent that will not start, say).
  document.getElementById('chat-auto-accept').onclick=()=>sendChat({type:'auto_accept',enabled:!chatState.autoAccept});
  const retry=document.getElementById('chat-retry');retry.hidden=!(session.executor!=='action'&&jobWorkedBy(job,currentOperator()));retry.disabled=false;retry.innerHTML=`${icon('replay')}Retry`;retry.dataset.retrySession=sessionID;retry.onclick=async()=>{await retrySession(retry);if(!retry.disabled)return;closeDialog('chat-dialog');};
  renderChatDeliverables(sessionID);renderChatQuestion(sessionID);openDialog('chat-dialog');connectACPChat(sessionID);requestAnimationFrame(()=>document.getElementById('chat-input').focus());
}
function closeACPChat(){chatState.manualClose=true;clearTimeout(chatState.reconnectTimer);clearTimeout(chatState.changeTimer);if(chatState.socket)chatState.socket.close();chatState.socket=null;chatState.busy=false;closeDiff();}
function bindACPButtons(root=document){root.querySelectorAll('[data-open-acp]').forEach(button=>button.onclick=()=>openACPChat(button.dataset.openAcp));}
function latestDeliverables(jobID){const latest=new Map();snapshot.deliverables.filter(item=>item.job_id===jobID).forEach(item=>{const current=latest.get(item.name);if(!current||item.revision>current.revision)latest.set(item.name,item);});return [...latest.values()];}
function jobAttachments(jobID){return snapshot.job_attachments.filter(item=>item.job_id===jobID);}
function attachmentIcon(item){return item.media_type==='application/pdf'?'picture_as_pdf':'image';}
function formatBytes(value){const bytes=Number(value||0);if(bytes<1024)return `${bytes} B`;if(bytes<1024*1024)return `${(bytes/1024).toFixed(bytes<10240?1:0)} KiB`;return `${(bytes/1024/1024).toFixed(1)} MiB`;}
function attachmentHTML(item){return `<a class="attachment-chip" href="/api/job-attachments/${encodeURIComponent(item.id)}" target="_blank" rel="noopener" title="Open ${esc(item.name)} · capsule: ${esc(item.capsule_path)}"><span class="material-symbols-outlined" aria-hidden="true">${attachmentIcon(item)}</span><span class="attachment-chip-name">${esc(item.name)}</span><span class="attachment-size">${esc(formatBytes(item.size))}</span></a>`;}
function deliverableRevisions(item){return snapshot.deliverables.filter(candidate=>candidate.job_id===item.job_id&&normalize(candidate.name)===normalize(item.name)).sort((a,b)=>a.revision-b.revision);}
function clearDeliverableHighlight(){deliverableView.ranges.clear();}
function textRange(root,start,end,exact=''){
  const text=root.textContent||'';let resolvedStart=Number(start),resolvedEnd=Number(end);
  if(resolvedStart<0||resolvedEnd<=resolvedStart||text.slice(resolvedStart,resolvedEnd)!==exact){const found=exact?text.indexOf(exact):-1;if(found<0)return null;resolvedStart=found;resolvedEnd=found+exact.length;}
  const walker=document.createTreeWalker(root,NodeFilter.SHOW_TEXT),nodes=[];let node,total=0;while((node=walker.nextNode())){nodes.push({node,start:total,end:total+node.data.length});total+=node.data.length;}
  const point=(offset,isEnd)=>{for(const entry of nodes){if(offset<entry.end||offset===entry.end&&isEnd)return {node:entry.node,offset:Math.max(0,offset-entry.start)};}const last=nodes.at(-1);return last?{node:last.node,offset:last.node.data.length}:null;};
  const from=point(resolvedStart,false),to=point(resolvedEnd,true);if(!from||!to)return null;const range=document.createRange();range.setStart(from.node,from.offset);range.setEnd(to.node,to.offset);return range;
}
function renderDeliverableHighlights(item,comments){
  clearDeliverableHighlight();const root=document.getElementById('deliverable-content'),walker=document.createTreeWalker(root,NodeFilter.SHOW_TEXT),nodes=[];let node,total=0;
  while((node=walker.nextNode())){nodes.push({node,start:total,end:total+node.data.length});total+=node.data.length;}
  nodes.forEach(entry=>{const intersections=comments.map(comment=>({comment,start:Math.max(entry.start,comment.start_offset),end:Math.min(entry.end,comment.end_offset)})).filter(part=>part.end>part.start);if(!intersections.length)return;const boundaries=[0,entry.node.data.length];intersections.forEach(part=>boundaries.push(part.start-entry.start,part.end-entry.start));const points=[...new Set(boundaries)].sort((a,b)=>a-b),fragment=document.createDocumentFragment();for(let index=0;index<points.length-1;index++){const start=points[index],end=points[index+1],value=entry.node.data.slice(start,end),active=intersections.filter(part=>part.start-entry.start<end&&part.end-entry.start>start).map(part=>part.comment.id);if(!active.length){fragment.append(document.createTextNode(value));continue;}const mark=document.createElement('mark');mark.className='deliverable-comment-mark';mark.dataset.commentIds=active.join(' ');mark.textContent=value;fragment.append(mark);}entry.node.replaceWith(fragment);});
  comments.forEach(comment=>{const range=textRange(root,comment.start_offset,comment.end_offset,comment.selected_text);if(range)deliverableView.ranges.set(comment.id,range);});
}
function cancelDeliverableComment(){deliverableView.selection=null;document.getElementById('deliverable-comment-form').hidden=true;document.getElementById('deliverable-comment-form').reset();window.getSelection()?.removeAllRanges();}
function captureDeliverableSelection(){
  const item=byID(snapshot.deliverables,deliverableView.id);if(!item)return;const revisions=deliverableRevisions(item),latest=revisions.at(-1);if(latest?.id!==item.id)return;
  const root=document.getElementById('deliverable-content'),selection=window.getSelection();if(!selection||selection.rangeCount!==1||selection.isCollapsed)return;const range=selection.getRangeAt(0),inside=node=>node===root||root.contains(node.nodeType===Node.TEXT_NODE?node.parentNode:node);if(!inside(range.startContainer)||!inside(range.endContainer))return;
  const exact=selection.toString();if(!exact.trim())return;const before=document.createRange();before.selectNodeContents(root);before.setEnd(range.startContainer,range.startOffset);const start=before.toString().length,end=start+exact.length,text=root.textContent||'';
  deliverableView.selection={selected_text:exact,start_offset:start,end_offset:end,prefix:text.slice(Math.max(0,start-120),start),suffix:text.slice(end,end+120)};
  document.getElementById('deliverable-comment-quote').textContent=exact;document.getElementById('deliverable-comment-form').hidden=false;requestAnimationFrame(()=>document.getElementById('deliverable-comment-body').focus());
}
function focusDeliverableComment(id){const range=deliverableView.ranges.get(id);if(!range)return;document.querySelectorAll('.comment-card.active,.deliverable-comment-mark.active').forEach(element=>element.classList.remove('active'));document.querySelector(`[data-focus-comment="${CSS.escape(id)}"]`)?.classList.add('active');const marks=[...document.querySelectorAll('.deliverable-comment-mark')].filter(mark=>String(mark.dataset.commentIds||'').split(' ').includes(id));marks.forEach(mark=>mark.classList.add('active'));marks[0]?.scrollIntoView({behavior:'smooth',block:'center'});if(document.activeElement instanceof HTMLElement)document.activeElement.blur();requestAnimationFrame(()=>{const selection=window.getSelection();selection.removeAllRanges();selection.addRange(range);});}
function renderDeliverableRevision(id){
  const item=byID(snapshot.deliverables,id);if(!item)return;deliverableView.id=item.id;cancelDeliverableComment();clearDeliverableHighlight();const revisions=deliverableRevisions(item),latest=revisions.at(-1),isLatest=latest?.id===item.id,comments=snapshot.deliverable_comments.filter(comment=>comment.deliverable_id===item.id);
  document.getElementById('deliverable-title').textContent=item.name;document.getElementById('deliverable-meta').textContent=`revisie ${item.revision} · ${item.description||'Session-bijlage'}`;
  const revisionRoot=document.getElementById('deliverable-revisions');revisionRoot.innerHTML=revisions.map(revision=>`<button type="button" class="revision-button ${revision.id===item.id?'active':''}" data-deliverable-revision="${esc(revision.id)}">r${revision.revision}${revision.id===latest.id?' · latest':''}</button>`).join('');revisionRoot.querySelectorAll('[data-deliverable-revision]').forEach(button=>button.onclick=()=>renderDeliverableRevision(button.dataset.deliverableRevision));
  const lock=document.getElementById('deliverable-lock');lock.className=`document-lock ${isLatest?'current':''}`;lock.textContent=isLatest?'Laatste revisie · selecteer tekst om te annoteren':'Historische revisie · alleen lezen';
  const visual=item.kind&&item.kind!=='markdown'&&item.bundle,download=document.getElementById('deliverable-download'),fileName=`${deliverableFileName(item.name)}-r${item.revision}.${visual?'zip':'md'}`;download.href=`/api/deliverables/${encodeURIComponent(item.id)}/download`;download.download=fileName;document.getElementById('deliverable-download-name').textContent=fileName;
  const content=document.getElementById('deliverable-content');content.classList.toggle('annotatable',isLatest&&!visual);content.classList.toggle('visual',Boolean(visual));
  let richContentReady;
  if(visual){const previewURL=`/preview/${encodeURIComponent(item.id)}/`,bundle=item.bundle,shape=bundle.folder?`map · ${bundle.files} bestand${bundle.files===1?'':'en'}${bundle.entry?' · index.html':''}`:(bundle.content_type||'bestand'),kind=item.kind;
    const view=kind==='image'?`<div class="preview-image"><img src="${previewURL}" alt="${esc(item.name)} revisie ${item.revision}"></div>`:kind==='pdf'?`<iframe class="preview-frame" src="${previewURL}" title="${esc(item.name)} revisie ${item.revision}"></iframe>`:kind==='file'?`<div class="preview-file">${icon('draft')}<strong>${esc(bundle.entry||'bestand')}</strong><span class="hint">${esc(bundle.content_type||'')}</span><a class="small-button" href="${download.href}" download="${esc(fileName)}">${icon('download')}Download</a></div>`:`<iframe class="preview-frame" src="${previewURL}" sandbox="allow-scripts allow-forms allow-modals" referrerpolicy="no-referrer" title="${esc(item.name)} revisie ${item.revision}"></iframe>`;
    content.innerHTML=`<div class="preview-bar"><span class="hint">${esc(shape)} · ${esc(formatBytes(bundle.size))}</span><a class="small-button" href="${previewURL}" target="_blank" rel="noopener">${icon('open_in_new')}Open in tabblad</a></div>${view}`;richContentReady=Promise.resolve();content.onmouseup=null;lock.textContent=isLatest?'Laatste revisie · commentaar geldt voor de hele revisie':'Historische revisie · alleen lezen';}
  else{richContentReady=setMarkdown(content,item.content);content.onmouseup=isLatest?()=>setTimeout(captureDeliverableSelection):null;}
  const list=document.getElementById('deliverable-comments');list.innerHTML=comments.length?comments.map(comment=>`<button type="button" class="comment-card" data-focus-comment="${esc(comment.id)}"><strong>${esc(comment.author)}</strong><small>${esc(new Date(comment.created_at).toLocaleString('nl-NL'))}</small>${comment.selected_text?`<blockquote>${esc(comment.selected_text)}</blockquote>`:''}<p>${esc(comment.body)}</p></button>`).join(''):`<div class="comment-history-note">${isLatest?'Selecteer tekst in het document om de eerste comment te plaatsen.':'Bij deze revisie zijn geen comments geplaatst.'}</div>`;
  list.querySelectorAll('[data-focus-comment]').forEach(button=>button.onclick=()=>focusDeliverableComment(button.dataset.focusComment));document.getElementById('deliverable-comment-help').textContent=comments.length?`${comments.length} comment${comments.length===1?'':'s'} op deze revisie.`:(isLatest?(visual?'Plaats een comment op deze revisie.':'Selecteer tekst in de laatste revisie.'):'Historische comments zijn immutable.');
  const whole=document.getElementById('deliverable-comment-whole');whole.hidden=!(visual&&isLatest);whole.onclick=()=>{deliverableView.selection={selected_text:'',start_offset:0,end_offset:0,prefix:'',suffix:''};document.getElementById('deliverable-comment-quote').textContent=`${item.name} · revisie ${item.revision}`;document.getElementById('deliverable-comment-form').hidden=false;requestAnimationFrame(()=>document.getElementById('deliverable-comment-body').focus());};
  richContentReady.then(()=>{if(deliverableView.id===item.id&&!visual)renderDeliverableHighlights(item,comments);});
}
function deliverableFileName(name){const safe=String(name||'').trim().replace(/ /g,'_').replace(/[^A-Za-z0-9._-]/g,'');return safe||'deliverable';}
function openDeliverable(id){if(!byID(snapshot.deliverables,id))return;renderDeliverableRevision(id);openDialog('deliverable-dialog');}
function closeDeliverable(){cancelDeliverableComment();clearDeliverableHighlight();deliverableView.id='';}
function bindDeliverables(root=document){root.querySelectorAll('[data-deliverable]').forEach(button=>button.onclick=()=>openDeliverable(button.dataset.deliverable));}
function renderChatDeliverables(sessionID){const session=byID(snapshot.sessions,sessionID),items=latestDeliverables(session?.job_id),attachments=jobAttachments(session?.job_id),root=document.getElementById('chat-deliverables');root.innerHTML=attachments.map(attachmentHTML).join('')+items.map(item=>`<button class="deliverable-chip" type="button" data-deliverable="${esc(item.id)}">${esc(item.name)} · r${item.revision}</button>`).join('')||'<span class="hint">Nog geen bijlagen of deliverables.</span>';bindDeliverables(root);}
function workflowTargetLabel(template,target){if(!target)return 'onbekend';if(target==='DONE')return 'Job klaar';const phase=(template?.phases||[]).find(item=>item.id===target);return phase?.name||target;}
// An agent ask is a form: every question with the options the agent expects,
// and always room for the operator's own words. The whole form is answered at
// once and the same ACP Session resumes with the answers.
function agentQuestionFormHTML(question){
  if(!question.items?.length)return `<p>${esc(question.question)}</p>`;
  return `<form class="question-form" data-question-form="${esc(question.id)}">${question.items.map(item=>{const name=`${question.id}-${item.id}`,options=(item.options||[]).map(option=>`<label class="question-option"><input type="radio" name="${esc(name)}" value="${esc(option)}"><span>${esc(option)}</span></label>`).join('');
    return `<fieldset class="question-item" data-question-item="${esc(item.id)}"><legend>${esc(item.question)}</legend>${options}<label class="question-option other"><input type="radio" name="${esc(name)}" value="__other__" ${options?'':'checked hidden'}><span>${options?'Anders:':''}</span><input type="text" name="${esc(name)}-other" placeholder="${options?'eigen antwoord':'jouw antwoord'}" autocomplete="off"></label></fieldset>`;}).join('')}</form>`;
}
function readAgentQuestionForm(question,root){
  const form=root.querySelector(`[data-question-form="${CSS.escape(question.id)}"]`);if(!form)return null;const answers=[];
  for(const item of question.items||[]){const name=`${question.id}-${item.id}`,picked=form.querySelector(`input[type=radio][name="${CSS.escape(name)}"]:checked`),other=form.querySelector(`input[name="${CSS.escape(name)}-other"]`),value=picked?.value==='__other__'||!picked?String(other?.value||'').trim():picked.value;
    if(!value){(picked?.value==='__other__'||!picked?other:form.querySelector(`[data-question-item="${CSS.escape(item.id)}"]`))?.focus();showError(new Error(`Beantwoord eerst: ${item.question}`));return null;}
    answers.push({item_id:item.id,answer:value});}
  return answers;
}
async function answerQuestionForm(id,button){
  const question=byID(snapshot.workflow_questions,id),root=button.closest('.question-card')||document;if(!question)return;const answers=readAgentQuestionForm(question,root);if(!answers)return;
  markQuestionBusy(id,'answer');
  try{await api(`/api/workflow/questions/${encodeURIComponent(id)}/answer`,{method:'POST',body:JSON.stringify({action:'answer',answers})});document.querySelectorAll(`[data-chat-question="${CSS.escape(id)}"]`).forEach(node=>node.remove());await refresh(true);}
  catch(error){showError(error);await refresh(true);}
}
function workflowDecisionTitle(question){if(question.kind==='agent')return 'AI VRAAGT INPUT';if(question.kind==='action')return 'AFRONDEN MISLUKT';if(byID(snapshot.sessions,question.session_id)?.executor==='expose')return 'TEST-APP STAAT KLAAR';return question.outcome==='reject'?'AI REJECTED':'AI ACCEPTED';}
function workflowDecisionRoutes(question,template){if(question.kind==='action')return `<div class="decision-routes"><span class="decision-route accept">RETRY → ${esc(workflowTargetLabel(template,question.accept_target))}</span></div>`;return `<div class="decision-routes"><span class="decision-route accept">ACCEPT → ${esc(workflowTargetLabel(template,question.accept_target))}</span><span class="decision-route reject">REJECT → ${esc(workflowTargetLabel(template,question.reject_target))}</span></div>`;}
function workflowAgentDetail(run,question,outcome){if(question?.agent_detail)return question.agent_detail;if(outcome==='accept'&&run.summary&&run.summary!=='Accepted by user')return run.summary;if(question?.question){if(outcome==='reject'){const marker=question.question.indexOf(' keer. ');if(marker>=0)return question.question.slice(marker+7);}const marker=question.question.indexOf('. ');if(marker>=0)return question.question.slice(marker+2);}return outcome==='reject'?run.reject_reason:run.summary;}
function workflowAgentOutcomeHistory(run,approvals){
  const questions=(approvals||[]).slice().sort((a,b)=>Date.parse(a.created_at)-Date.parse(b.created_at)),byOutcomeID=new Map(questions.filter(item=>item.agent_outcome_id).map(item=>[item.agent_outcome_id,item])),usedQuestions=new Set(),history=[];
  (run.agent_outcomes||[]).forEach(item=>{const question=byOutcomeID.get(item.id);if(question)usedQuestions.add(question.id);history.push({outcome:item.outcome,detail:item.detail||workflowAgentDetail(run,question,item.outcome),created_at:item.created_at,question});});
  questions.filter(question=>!usedQuestions.has(question.id)).forEach(question=>history.push({outcome:question.outcome,detail:workflowAgentDetail(run,question,question.outcome),created_at:question.created_at,question}));
  history.sort((a,b)=>Date.parse(a.created_at)-Date.parse(b.created_at));
  if(!(run.agent_outcomes||[]).length){const terminalOutcome=run.status==='accepted'?'accept':run.status==='rejected'?'reject':'',last=history.at(-1),continued=last?.question?.status==='answered'&&['chat','retry'].includes(last.question.answer);if(terminalOutcome&&(!last||continued))history.push({outcome:terminalOutcome,detail:terminalOutcome==='accept'?run.summary:run.reject_reason,created_at:run.completed_at||new Date().toISOString(),question:null});}
  return history;
}
function workflowHumanDetail(question){if(question.reason)return question.reason;return ({accept:'Geen aanvullende toelichting.',reject:'Geen reden vastgelegd.',chat:'Chat hervatte dezelfde Session.',retry:'Session opnieuw gestart.'})[question.answer]||'Geen aanvullende toelichting.';}
function workflowDecisionHistory(run,approvals,job){const history=workflowAgentOutcomeHistory(run,approvals);if(!history.length)return '';const entries=history.map(item=>{const question=item.question,answered=question?.status==='answered'&&['accept','reject','chat','retry'].includes(question.answer),operator=(question?.answered_by||job.owner||'USER').toUpperCase(),next=question?.answer==='accept'?'VERDER':question?.answer==='chat'?'CHAT':'OPNIEUW';return `<div class="decision-entry ai ${esc(item.outcome)}"><strong>AI ${esc(item.outcome.toUpperCase())}</strong><div class="decision-text">${markdown(item.detail||'Geen toelichting.')}</div></div>${answered?`<div class="decision-entry human ${esc(question.answer)}"><strong>${esc(operator)} ${esc(question.answer.toUpperCase())}</strong><div class="decision-text">${markdown(workflowHumanDetail(question))}</div><span class="decision-next ${esc(question.answer)}">${icon('arrow_forward')}${next}</span></div>`:''}`;}).join('');return `<div class="decision-history">${history.length>1?`<div class="decision-history-head">Besluitverloop · ${history.length} AI-uitkomsten</div>`:''}${entries}</div>`;}
// The decision card stays in the chat while the operator talks to the agent.
// Its buttons close while the agent works and reopen when the turn ends; a new
// decision from the agent replaces the card on the next refresh.
function rerenderChatQuestion(){document.querySelectorAll('[data-chat-question]').forEach(node=>node.remove());if(chatState.sessionID)renderChatQuestion(chatState.sessionID);}
function renderChatQuestion(sessionID){const question=snapshot.workflow_questions.find(item=>item.session_id===sessionID&&item.status==='open');if(!question)return;const session=byID(snapshot.sessions,sessionID),job=byID(snapshot.jobs,session?.job_id),run=byID(snapshot.phase_runs,question.phase_run_id),template=job?jobTemplate(job):null,approvals=snapshot.workflow_questions.filter(item=>item.phase_run_id===run?.id&&item.kind==='approval'),node=document.createElement('section');node.className='question-card';node.dataset.chatQuestion=question.id;const form=question.kind==='agent'&&question.items?.length;node.innerHTML=`<strong>${workflowDecisionTitle(question)}</strong>${question.kind==='approval'&&run?workflowDecisionHistory(run,approvals,job):agentQuestionFormHTML(question)}${workflowDecisionRoutes(question,template)}<small class="question-hint" data-question-busy hidden>${icon('progress_activity')}De agent is bezig · beslissen kan zodra de beurt eindigt</small><div class="panel-actions">${form?`<button class="primary" data-answer-question="${esc(question.id)}">BEANTWOORD</button>`:''}${acceptButtons(question,form?'small-button':'primary','')}<button class="${form?'small-button':'danger'}" data-reject-question="${esc(question.id)}">REJECT</button></div>`;chatAppend(node);bindDetailStates(node);bindQuestionButtons(node);renderChatBusy();}
function markQuestionBusy(id,action){document.querySelectorAll(`[data-chat-question="${CSS.escape(id)}"],[data-workflow-question="${CSS.escape(id)}"]`).forEach(node=>{node.querySelectorAll('button').forEach(button=>button.disabled=true);const actions=node.querySelector('.panel-actions');if(actions)actions.innerHTML=`<span class="tag status-busy">${esc(action.toUpperCase())} wordt verwerkt…</span>`;});}
async function decideQuestion(id,action,reason=''){const question=byID(snapshot.workflow_questions,id);markQuestionBusy(id,action);closeDialog('reject-dialog');if(document.getElementById('job-changes-dialog').open&&jobChangesState.bundle?.revision?.context_phase_run_id===question?.phase_run_id)closeDialog('job-changes-dialog');pendingRejectQuestionID='';try{const advance=await api(`/api/workflow/questions/${encodeURIComponent(id)}/answer`,{method:'POST',body:JSON.stringify({action,reason})});if(document.getElementById('chat-dialog').open&&chatState.sessionID===question?.session_id)closeDialog('chat-dialog');await refresh(true);if(advance.next_session)showNotice(`${advance.next_session.role||'Volgende fase'} staat queued en start op de achtergrond.`);}catch(error){showError(error);await refresh(true);}}
// A Job runs on a copy of its Template; a newer revision is taken over on
// request, keeping the current step or continuing at a chosen one.
// The environment a Job's next steps start with can change: a wrong pick
// at creation is not for ever.
let jobEnvironmentJobID='';
function openJobEnvironment(jobID){const job=byID(snapshot.jobs,jobID);if(!job)return;jobEnvironmentJobID=jobID;document.getElementById('job-environment-copy').textContent=`${job.title} start zijn volgende stappen met deze agent en connecties.`;fillEnvironmentSelect(document.getElementById('job-environment-selector'),job.environment_selector||'',capabilitySelectors('acp'));fillMCPSelect(document.getElementById('job-environment-mcp'),job.mcp_server_ids||[]);openDialog('job-environment-dialog');}
document.getElementById('job-environment-form').onsubmit=async event=>{event.preventDefault();if(!jobEnvironmentJobID)return;const form=new FormData(event.target);try{await api(`/api/jobs/${encodeURIComponent(jobEnvironmentJobID)}/environment`,{method:'PUT',body:JSON.stringify({environment_selector:String(form.get('environment_selector')||''),mcp_server_ids:selectedValues(document.getElementById('job-environment-mcp'))})});closeDialog('job-environment-dialog');showNotice('Omgeving van de Job aangepast voor de volgende stappen');await refresh(true);}catch(error){showError(error);}};
let adoptTemplateJobID='';
function openAdoptTemplate(jobID){const job=byID(snapshot.jobs,jobID),latest=job&&byID(snapshot.workflow_templates,job.template_id);if(!job||!latest)return;adoptTemplateJobID=jobID;const current=jobTemplate(job),run=byID(snapshot.phase_runs,job.current_phase_run_id),currentExists=run&&(latest.phases||[]).some(phase=>phase.id===run.phase_id);
  document.getElementById('adopt-template-copy').textContent=`${esc(job.title)} draait op ${current?.name||'het Template'} r${current?.revision||1}; revisie ${latest.revision} overnemen.`;
  const select=document.getElementById('adopt-template-phase');select.innerHTML=(latest.phases||[]).map((phase,index)=>`<option value="${esc(phase.id)}" ${currentExists&&phase.id===run.phase_id?'selected':''}>Verder bij ${index+1}. ${esc(phase.name)}</option>`).join('');
  openDialog('adopt-template-dialog');}
document.getElementById('adopt-template-form').onsubmit=async event=>{event.preventDefault();if(!adoptTemplateJobID)return;const phase=document.getElementById('adopt-template-phase').value;try{await api(`/api/jobs/${encodeURIComponent(adoptTemplateJobID)}/template`,{method:'POST',body:JSON.stringify({phase_id:phase})});closeDialog('adopt-template-dialog');showNotice('Job gaat verder op de nieuwe Template-revisie');await refresh(true);}catch(error){showError(error);}};
function openRejectDecision(id){pendingRejectQuestionID=id;document.getElementById('reject-form').reset();openDialog('reject-dialog');requestAnimationFrame(()=>document.getElementById('reject-reason').focus());}
// The Template decides what ACCEPT means, including how the Job lands;
// a person only accepts or rejects.
function acceptButtons(question,cls,lock){return `<button class="${cls}" data-accept-question="${esc(question?.id)}"${lock}>${icon('check')}ACCEPT</button>`;}
function bindQuestionButtons(root=document){root.querySelectorAll('[data-accept-question]').forEach(button=>button.onclick=()=>decideQuestion(button.dataset.acceptQuestion,'accept'));root.querySelectorAll('[data-reject-question]').forEach(button=>button.onclick=()=>openRejectDecision(button.dataset.rejectQuestion));root.querySelectorAll('[data-answer-question]').forEach(button=>button.onclick=()=>answerQuestionForm(button.dataset.answerQuestion,button));
  root.querySelectorAll('.question-option.other input[type=text]').forEach(input=>{const select=()=>{const radio=input.closest('label')?.querySelector('input[type=radio]');if(radio)radio.checked=true;};input.onfocus=select;input.oninput=select;});
  root.querySelectorAll('.question-form').forEach(form=>form.onsubmit=event=>{event.preventDefault();form.closest('.question-card')?.querySelector('[data-answer-question]')?.click();});}
function scheduleChatChanges(now=false){clearTimeout(chatState.changeTimer);chatState.changeTimer=setTimeout(refreshChatChanges,now?0:650);}
async function refreshChatChanges(){if(!chatState.sessionID)return;try{renderChanges(await api(`/api/sessions/${encodeURIComponent(chatState.sessionID)}/changes`));}catch(error){document.getElementById('changes-branch').textContent=error.message||'Changes unavailable';}}
function renderChanges(changes){
  const files=Array.isArray(changes.files)?changes.files:[];chatState.changes={...changes,files};document.getElementById('changes-added').textContent=`+${changes.added||0}`;document.getElementById('changes-deleted').textContent=`−${changes.deleted||0}`;const syncedSession=byID(snapshot.sessions,chatState.sessionID),synced=syncedSession?.synced_at?` · op remote ${String(syncedSession.synced_head||'').slice(0,7)} ${chatClock(syncedSession.synced_at)}`:'';document.getElementById('changes-branch').textContent=(changes.branch||'detached workspace')+synced;
  const list=document.getElementById('changes-list');list.innerHTML=files.length?files.map(file=>`<button type="button" class="change-file${chatState.selectedDiffPath===file.path?' active':''}" data-diff-path="${esc(file.path)}" title="${esc(file.path)}"><span class="change-status ${changeStatusClass(file.status)}">${esc(String(file.status||'M').trim().charAt(0)||'M')}</span><span class="change-path" title="${esc(file.path)}">${esc(shortPath(file.path))}</span><span class="change-count"><span class="diff-add">+${file.added||0}</span> <span class="diff-del">−${file.deleted||0}</span></span></button>`).join(''):'<div class="empty">Nog geen wijzigingen.</div>';
  list.querySelectorAll('[data-diff-path]').forEach(button=>button.onclick=()=>openDiff(button.dataset.diffPath));if(chatState.selectedDiffPath)openDiff(chatState.selectedDiffPath);
}
function parseUnifiedPatch(patch){
  const hunks=[];let hunk=null;
  String(patch||'').split('\n').forEach(line=>{
    const match=line.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$/);
    if(match){hunk={header:line,oldLine:Number(match[1]),newLine:Number(match[3]),lines:[]};hunks.push(hunk);return;}
    if(!hunk||/^diff --git |^index |^--- |^\+\+\+ /.test(line))return;
    if(line.startsWith(' ')){hunk.lines.push({type:'context',text:line.slice(1),old:hunk.oldLine++,new:hunk.newLine++});return;}
    if(line.startsWith('-')){hunk.lines.push({type:'delete',text:line.slice(1),old:hunk.oldLine++,new:null});return;}
    if(line.startsWith('+')){hunk.lines.push({type:'add',text:line.slice(1),old:null,new:hunk.newLine++});return;}
    if(line.startsWith('\\ No newline'))hunk.lines.push({type:'note',text:line,old:null,new:null});
  });return hunks;
}
function codeCommentsAtLine(comments,side,line){return (comments||[]).filter(comment=>comment.side===side&&line>=comment.start_line&&line<=comment.end_line);}
function diffCell(line,side,language,comments=[]){
  if(!line)return '<div class="diff-cell diff-cell-empty"><span class="line-number"></span><code></code></div>';
  const number=side==='old'?line.old:line.new,matching=number?codeCommentsAtLine(comments,side,number):[],commentIDs=matching.map(comment=>comment.id).join(' ');return `<div class="diff-cell ${esc(line.type)}${matching.length?' has-code-comment':''}"${number?` data-review-side="${side}" data-review-line="${number}"`:''}${commentIDs?` data-code-comment-ids="${esc(commentIDs)}"`:''}><span class="line-number">${number??''}</span><code>${highlightCode(line.text,language)}</code></div>`;
}
function renderSplitDiff(hunks,language,comments=[]){
  const rows=['<div class="diff-side-head"><span>Before</span><span>After</span></div>'];
  hunks.forEach(hunk=>{rows.push(`<div class="diff-hunk">${esc(hunk.header)}</div>`);for(let index=0;index<hunk.lines.length;){const line=hunk.lines[index];
    if(line.type==='context'){rows.push(`<div class="diff-side-row">${diffCell(line,'old',language,comments)}${diffCell(line,'new',language,comments)}</div>`);index++;continue;}
    if(line.type==='note'){rows.push(`<div class="diff-hunk">${esc(line.text)}</div>`);index++;continue;}
    const deletes=[],adds=[];while(index<hunk.lines.length&&!['context','note'].includes(hunk.lines[index].type)){const changed=hunk.lines[index++];(changed.type==='delete'?deletes:adds).push(changed);}
    const count=Math.max(deletes.length,adds.length);for(let offset=0;offset<count;offset++)rows.push(`<div class="diff-side-row">${diffCell(deletes[offset],'old',language,comments)}${diffCell(adds[offset],'new',language,comments)}</div>`);
  }});return `<div class="diff-split">${rows.join('')}</div>`;
}
function renderUnifiedDiff(hunks,language,comments=[]){
  const rows=[];hunks.forEach(hunk=>{rows.push(`<div class="diff-hunk">${esc(hunk.header)}</div>`);hunk.lines.forEach(line=>{
    if(line.type==='note'){rows.push(`<div class="diff-hunk">${esc(line.text)}</div>`);return;}const prefix=line.type==='add'?'+':line.type==='delete'?'-':' ';
    const side=line.type==='delete'?'old':'new',number=side==='old'?line.old:line.new,matching=number?codeCommentsAtLine(comments,side,number):[],commentIDs=matching.map(comment=>comment.id).join(' ');rows.push(`<div class="diff-row ${esc(line.type)}${matching.length?' has-code-comment':''}"${number?` data-review-side="${side}" data-review-line="${number}"`:''}${commentIDs?` data-code-comment-ids="${esc(commentIDs)}"`:''}><span class="line-number">${line.old??''}</span><span class="line-number">${line.new??''}</span><code><span class="diff-prefix">${prefix}</span>${highlightCode(line.text,language)}</code></div>`);
  });});return `<div class="diff-unified">${rows.join('')}</div>`;
}
function renderDiff(file,comments=[]){
  const warnings=[];if(file.binary)warnings.push('Binary file · tekst-diff niet beschikbaar');if(file.truncated)warnings.push('Grote diff · weergave is afgekapt');const hunks=parseUnifiedPatch(file.patch),language=syntaxLanguage(file.path);
  return `${warnings.map(warning=>`<div class="diff-warning">${esc(warning)}</div>`).join('')}${hunks.length?renderSplitDiff(hunks,language,comments)+renderUnifiedDiff(hunks,language,comments):'<div class="diff-empty">Voor dit bestand is geen tekst-diff beschikbaar.</div>'}`;
}
function openDiff(path){
  const normalized=workspacePath(path),file=chatState.changes.files.find(candidate=>workspacePath(candidate.path)===normalized);chatState.selectedDiffPath=normalized;
  document.querySelectorAll('[data-diff-path]').forEach(button=>button.classList.toggle('active',workspacePath(button.dataset.diffPath)===normalized));const drawer=document.getElementById('diff-drawer');drawer.hidden=false;
  document.getElementById('diff-file-name').textContent=shortPath(file?.path||normalized||'Change',72);document.getElementById('diff-file-name').title=file?.path||normalized||'';document.getElementById('diff-file-meta').textContent=file?`${file.status||'M'} · +${file.added||0} −${file.deleted||0} · side by side op brede schermen, inline op smalle`:'Wijziging wordt uit de Git-worktree geladen…';document.getElementById('diff-view').innerHTML=file?renderDiff(file):'<div class="diff-empty">Deze wijziging is nog niet zichtbaar in Git. De worktree wordt opnieuw gelezen…</div>';
}
function closeDiff(){chatState.selectedDiffPath='';const drawer=document.getElementById('diff-drawer');if(drawer)drawer.hidden=true;document.querySelectorAll('[data-diff-path]').forEach(button=>button.classList.remove('active'));
}
// The code browser: a Job's files straight from Git, folders as a tree,
// one file at a time. Nothing is stored; it reads the latest running
// workspace of the Job.
// The Explore browser: a repository through a runner's clone. A view
// names the DOM it draws into.
const codeViews={repo:{tree:'explore-tree',file:'explore-file',name:'explore-file-name',meta:'explore-file-meta',ref:'explore-ref',context:''}};
const codeState={source:null,ref:'',refs:[],entries:[],path:''};
function codeURL(mode,params={}){const query=new URLSearchParams(params).toString();return `/api/git/repositories/${encodeURIComponent(codeState.source.id)}/code/${mode}?${query}`;}
function codeView(){return codeViews.repo;}
function codeEl(key){const id=codeView()[key];return id?document.getElementById(id):null;}
async function exploreRepository(repositoryID){
  const repository=byID(snapshot.git_repositories,repositoryID);if(!repository)return;codeState.source={type:'repo',id:repositoryID};codeState.path='';codeState.ref=repository.default_ref||'';
  codeEl('tree').innerHTML='<div class="empty">Branches ophalen…</div>';codeEl('file').innerHTML='<div class="diff-empty">Kies links een bestand.</div>';codeEl('name').textContent='Selecteer een bestand';codeEl('meta').textContent='Gelezen uit Git.';
  try{const refs=await api(codeURL('refs')),named=(refs.refs||[]).map(ref=>typeof ref==='string'?{name:ref}:ref);named.sort((a,b)=>(a.name===refs.default_ref?-1:b.name===refs.default_ref?1:0));codeState.refs=named;if(!named.some(ref=>ref.name===codeState.ref))codeState.ref=refs.default_ref||named[0]?.name||'';}
  catch(error){codeEl('tree').innerHTML=`<div class="empty">${esc(error.message||error)}</div>`;return;}
  renderCodeRefs();await loadCodeTree();
}
// Branches grouped by their prefix (feature/, fix/, …), each group newest
// first; branches without a prefix stay at the top, the default first.
function renderCodeRefs(){const select=codeEl('ref');if(!select)return;
  const items=codeState.refs.map(ref=>typeof ref==='string'?{name:ref}:ref),option=item=>{const age=item.committed_at?` · ${elapsedSince(item.committed_at)}`:'';return `<option value="${esc(item.name)}">${esc(item.name==='workspace'?'workspace · werkbestanden':item.name)}${esc(age)}</option>`;};
  const plain=items.filter(item=>!item.name.includes('/')),groups=new Map();items.filter(item=>item.name.includes('/')).forEach(item=>{const prefix=item.name.slice(0,item.name.indexOf('/'));if(!groups.has(prefix))groups.set(prefix,[]);groups.get(prefix).push(item);});
  select.innerHTML=plain.map(option).join('')+[...groups.entries()].map(([prefix,members])=>`<optgroup label="${esc(prefix)}/ · ${members.length}">${members.map(option).join('')}</optgroup>`).join('');
  select.value=codeState.ref;select.onchange=()=>{codeState.ref=select.value;codeState.path='';loadCodeTree();};}
async function loadCodeTree(){
  codeEl('tree').innerHTML='<div class="empty">Bestanden laden…</div>';
  try{const tree=await api(codeURL('tree',{ref:codeState.ref}));codeState.ref=tree.ref;codeState.entries=tree.entries||[];if(tree.refs){codeState.refs=tree.refs;renderCodeRefs();}
    const context=codeEl('context');if(context)context.textContent=`${codeState.entries.length} bestanden · ${tree.ref}`;renderCodeTree();
  }catch(error){const context=codeEl('context');if(context)context.textContent=error.message||'Bestanden niet beschikbaar';codeEl('tree').innerHTML=`<div class="empty">${esc(error.message||error)}</div>`;}
}
function codeTreeModel(entries){const root={dirs:new Map(),files:[]};entries.forEach(entry=>{const parts=entry.path.split('/');let node=root;parts.slice(0,-1).forEach(part=>{if(!node.dirs.has(part))node.dirs.set(part,{dirs:new Map(),files:[]});node=node.dirs.get(part);});node.files.push({name:parts.at(-1),path:entry.path,size:entry.size});});return root;}
// fileIcon picks a Material Symbol for a file by its name.
function fileIcon(name){const lower=String(name).toLowerCase(),ext=lower.includes('.')?lower.slice(lower.lastIndexOf('.')+1):'';
  if(/^(readme|changelog|license|licence|contributing)/.test(lower))return 'article';
  if(lower==='dockerfile'||lower.startsWith('dockerfile.'))return 'deployed_code';
  if(lower==='makefile'||lower==='justfile')return 'build';
  return {js:'javascript',mjs:'javascript',cjs:'javascript',ts:'javascript',tsx:'javascript',jsx:'javascript',html:'html',htm:'html',css:'css',scss:'css',less:'css',json:'data_object',yaml:'data_object',yml:'data_object',toml:'data_object',xml:'data_object',md:'article',txt:'notes',go:'code',rs:'code',py:'code',rb:'code',php:'code',java:'code',kt:'code',swift:'code',c:'code',h:'code',cpp:'code',cs:'code',cshtml:'html',razor:'html',sql:'database',sh:'terminal',bash:'terminal',zsh:'terminal',ps1:'terminal',bat:'terminal',png:'image',jpg:'image',jpeg:'image',gif:'image',webp:'image',svg:'image',ico:'image',pdf:'picture_as_pdf',zip:'folder_zip',gz:'folder_zip',tar:'folder_zip',env:'key',lock:'lock',csproj:'settings',sln:'settings',mod:'settings',sum:'lock',gitignore:'visibility_off',editorconfig:'settings'}[ext]||'description';}
// Folders are plain rows toggled by hand (no <details>: browsers draw
// their own marker there). Which folders are open is remembered per path;
// the top level starts open.
const codeOpenFolders=new Set();
function renderCodeNode(node,depth,prefix){const dirs=[...node.dirs.entries()].sort((a,b)=>a[0].localeCompare(b[0])),files=node.files.sort((a,b)=>a.name.localeCompare(b.name));
  return dirs.map(([name,child])=>{const path=prefix?`${prefix}/${name}`:name,open=codeOpenFolders.has(path)||(depth<1&&!codeOpenFolders.has('!'+path));return `<div class="tree-folder${open?' open':''}" data-folder="${esc(path)}"><button type="button" class="tree-row" data-toggle-folder="${esc(path)}" title="${esc(path)}"><span class="material-symbols-outlined tree-slot" aria-hidden="true">chevron_right</span><span class="material-symbols-outlined tree-icon folder" aria-hidden="true">${open?'folder_open':'folder'}</span><span class="tree-name">${esc(name)}</span></button><div class="tree-children" ${open?'':'hidden'}>${renderCodeNode(child,depth+1,path)}</div></div>`;}).join('')+files.map(file=>`<button type="button" class="tree-row tree-file${codeState.path===file.path?' active':''}" data-code-path="${esc(file.path)}" title="${esc(file.path)}"><span class="tree-slot"></span><span class="material-symbols-outlined tree-icon" aria-hidden="true">${fileIcon(file.name)}</span><span class="tree-name">${esc(file.name)}</span>${file.size?`<span class="tree-size">${esc(formatBytes(file.size))}</span>`:''}</button>`).join('');}
function renderCodeTree(){const root=codeEl('tree');root.innerHTML=codeState.entries.length?renderCodeNode(codeTreeModel(codeState.entries),0,''):'<div class="empty">Geen bestanden in deze versie.</div>';
  root.querySelectorAll('[data-code-path]').forEach(button=>button.onclick=()=>openCodeFile(button.dataset.codePath));
  root.querySelectorAll('[data-toggle-folder]').forEach(button=>button.onclick=()=>{const path=button.dataset.toggleFolder,folder=button.parentElement,open=!folder.classList.contains('open');folder.classList.toggle('open',open);folder.querySelector(':scope>.tree-children').hidden=!open;button.querySelector('.tree-icon').textContent=open?'folder_open':'folder';if(open){codeOpenFolders.add(path);codeOpenFolders.delete('!'+path);}else{codeOpenFolders.delete(path);codeOpenFolders.add('!'+path);}});}
async function openCodeFile(path){
  codeState.path=path;codeEl('tree').querySelectorAll('[data-code-path]').forEach(button=>button.classList.toggle('active',button.dataset.codePath===path));
  const name=codeEl('name'),meta=codeEl('meta'),view=codeEl('file');name.textContent=shortPath(path,72);name.title=path;meta.textContent='Laden…';
  try{const file=await api(codeURL('file',{ref:codeState.ref,path}));
    meta.textContent=`${formatBytes(file.size||0)} · ${file.ref}${file.truncated?' · afgekapt op 512 KiB':''}`;
    if(file.binary){view.innerHTML='<div class="diff-empty">Binair bestand.</div>';return;}
    view.innerHTML=`<pre class="code-listing">${String(file.content||'').split('\n').map(line=>`<span class="code-line">${highlightCode(line,path)||' '}</span>`).join('')}</pre>`;
  }catch(error){meta.textContent=error.message||'Niet beschikbaar';view.innerHTML=`<div class="diff-empty">${esc(error.message||error)}</div>`;}
}
function renderExploreOptions(){const select=document.getElementById('explore-repository'),current=select.value;select.innerHTML='<option value="">kies een repository</option>'+snapshot.git_repositories.map(repository=>`<option value="${esc(repository.id)}">${esc(repository.name)}</option>`).join('');if(snapshot.git_repositories.some(repository=>repository.id===current))select.value=current;}
async function openJobChanges(button){
  const job=byID(snapshot.jobs,button.dataset.jobChanges),sessionID=button.dataset.changeSession||'',live=button.dataset.changeLive==='true',run=sessionID?snapshot.phase_runs.find(item=>item.session_id===sessionID):null;
  if(!job)return;jobChangesState.jobID=job.id;jobChangesState.sessionID=sessionID;jobChangesState.selectedPath='';jobChangesState.bundle=null;jobChangesState.selection=null;jobChangesState.changes={branch:'',added:0,deleted:0,files:[]};
  document.getElementById('job-changes-title').textContent=sessionID?`${run?.phase_name||'Stap'} changes`:`${job.title} changes`;
  document.getElementById('job-changes-context').textContent=sessionID?`Laatste diff van ${run?.phase_name||sessionID}; de Job-knop bovenin blijft de volledige boom tonen.`:`Volledige boom op ${job.branch} sinds ${job.base_ref}.`;
  document.getElementById('job-changes-added').textContent='+0';document.getElementById('job-changes-deleted').textContent='−0';document.getElementById('job-changes-file-count').textContent='0';document.getElementById('job-changes-branch').textContent='Reviewrevisie vastleggen…';document.getElementById('job-changes-list').innerHTML='<div class="empty">Changes laden…</div>';document.getElementById('job-change-file-name').textContent='Selecteer een bestand';document.getElementById('job-change-file-meta').textContent='Side by side op brede schermen · inline op smalle schermen';document.getElementById('job-change-diff').innerHTML='<div class="diff-empty">Git-history en worktree worden vergeleken…</div>';document.getElementById('code-review-revisions').innerHTML='';document.getElementById('code-review-comments').innerHTML='<div class="comment-history-note">Reviewrevisie laden…</div>';document.getElementById('code-review-decision').innerHTML='';cancelCodeReviewComment();openDialog('job-changes-dialog');
  try{const bundle=await api(`/api/jobs/${encodeURIComponent(job.id)}/code-reviews`,{method:'POST',body:JSON.stringify({session_id:sessionID,live})});renderCodeReviewBundle(bundle);}catch(error){document.getElementById('job-changes-branch').textContent='Vergelijking mislukt';document.getElementById('job-changes-list').innerHTML=`<div class="empty">${esc(error.message||error)}</div>`;document.getElementById('job-change-diff').innerHTML='<div class="diff-empty">De opgeslagen Git-workspace kon niet worden vergeleken.</div>';document.getElementById('code-review-comments').innerHTML='<div class="comment-history-note">Geen reviewrevisie beschikbaar.</div>';}
}
function codeReviewRevisionLabel(revision,index,latestID){if(revision.scope==='phase')return `poging ${revision.attempt}${revision.id===latestID?' · latest':''}`;return `r${index+1}${revision.id===latestID?' · latest':''}`;}
function renderCodeReviewBundle(bundle){
  jobChangesState.bundle=bundle;jobChangesState.selection=null;const revision=bundle.revision,history=Array.isArray(bundle.history)?bundle.history:[],comments=Array.isArray(bundle.comments)?bundle.comments:[];
  const revisions=document.getElementById('code-review-revisions');revisions.innerHTML=history.map((item,index)=>`<button type="button" class="revision-button ${item.id===revision.id?'active':''}" data-code-review-revision="${esc(item.id)}">${esc(codeReviewRevisionLabel(item,index,bundle.latest_revision_id))}</button>`).join('');revisions.querySelectorAll('[data-code-review-revision]').forEach(button=>button.onclick=()=>loadCodeReviewRevision(button.dataset.codeReviewRevision));
  document.getElementById('code-review-help').textContent=bundle.annotatable?'Laatste revisie · selecteer tekst of klik een coderegel.':'Historische revisie · comments blijven zichtbaar maar zijn immutable.';
  renderCodeReviewComments(comments);renderCodeReviewDecision(revision,bundle.annotatable);renderJobChanges({branch:revision.branch,added:revision.added,deleted:revision.deleted,files:revision.files});
}
async function loadCodeReviewRevision(id){if(jobChangesState.bundle?.revision?.id===id)return;cancelCodeReviewComment();try{renderCodeReviewBundle(await api(`/api/code-reviews/${encodeURIComponent(id)}`));}catch(error){showError(error);}}
function renderCodeReviewComments(comments){
  const root=document.getElementById('code-review-comments');root.innerHTML=comments.length?comments.map(comment=>`<button type="button" class="comment-card" data-focus-code-comment="${esc(comment.id)}"><strong title="${esc(comment.path)}">${esc(comment.author)} · <span class="comment-path">${esc(shortPath(comment.path,36))}</span>:${comment.start_line}${comment.end_line>comment.start_line?`-${comment.end_line}`:''}</strong><small>${esc(comment.side)} · ${esc(new Date(comment.created_at).toLocaleString('nl-NL'))}</small><blockquote>${esc(comment.selected_text)}</blockquote><p>${esc(comment.body)}</p></button>`).join(''):`<div class="comment-history-note">${jobChangesState.bundle?.annotatable?'Selecteer code om de eerste comment te plaatsen.':'Bij deze revisie zijn geen comments geplaatst.'}</div>`;
  root.querySelectorAll('[data-focus-code-comment]').forEach(button=>button.onclick=()=>focusCodeReviewComment(button.dataset.focusCodeComment));
}
function renderCodeReviewDecision(revision,annotatable){
  const root=document.getElementById('code-review-decision'),question=snapshot.workflow_questions.find(item=>item.status==='open'&&item.phase_run_id===revision.context_phase_run_id);
  if(!annotatable||!question){root.innerHTML='';return;}root.dataset.workflowQuestion=question.id;root.innerHTML=`<span>${icon('rate_review')} Review klaar? Comments gaan bij REJECT mee naar de volgende poging.</span><div class="panel-actions"><button class="primary" data-accept-question="${esc(question.id)}">${icon('check')}ACCEPT</button><button class="danger" data-reject-question="${esc(question.id)}">${icon('cancel')}REJECT</button></div>`;bindQuestionButtons(root);
}
function renderJobChanges(changes){
  const files=Array.isArray(changes.files)?changes.files:[];jobChangesState.changes={...changes,files};document.getElementById('job-changes-added').textContent=`+${changes.added||0}`;document.getElementById('job-changes-deleted').textContent=`−${changes.deleted||0}`;document.getElementById('job-changes-file-count').textContent=String(files.length);document.getElementById('job-changes-branch').textContent=changes.branch||'detached workspace';
  const list=document.getElementById('job-changes-list');list.innerHTML=files.length?files.map(file=>`<button type="button" class="change-file${workspacePath(file.path)===jobChangesState.selectedPath?' active':''}" data-job-diff-path="${esc(file.path)}" title="${esc(file.path)}"><span class="change-status ${changeStatusClass(file.status)}">${esc(String(file.status||'M').trim().charAt(0)||'M')}</span><span class="change-path" title="${esc(file.path)}">${esc(shortPath(file.path))}</span><span class="change-count"><span class="diff-add">+${file.added||0}</span> <span class="diff-del">−${file.deleted||0}</span></span></button>`).join(''):'<div class="empty">Deze vergelijking bevat geen codewijzigingen.</div>';
  list.querySelectorAll('[data-job-diff-path]').forEach(button=>button.onclick=()=>openJobChangeDiff(button.dataset.jobDiffPath));if(files.length){const selected=files.find(file=>workspacePath(file.path)===jobChangesState.selectedPath);openJobChangeDiff(selected?.path||files[0].path);}else{document.getElementById('job-change-file-name').textContent='Geen changes';document.getElementById('job-change-file-meta').textContent='Deze stap of Job veranderde geen bestanden.';document.getElementById('job-change-diff').innerHTML='<div class="diff-empty">Geen codewijzigingen gevonden.</div>';}
}
function openJobChangeDiff(path){
  const normalized=workspacePath(path),file=jobChangesState.changes.files.find(candidate=>workspacePath(candidate.path)===normalized),comments=(jobChangesState.bundle?.comments||[]).filter(comment=>workspacePath(comment.path)===normalized);jobChangesState.selectedPath=normalized;document.querySelectorAll('[data-job-diff-path]').forEach(button=>button.classList.toggle('active',workspacePath(button.dataset.jobDiffPath)===normalized));document.getElementById('job-change-file-name').textContent=file?.path||normalized||'Change';document.getElementById('job-change-file-meta').textContent=file?`${file.status||'M'} · +${file.added||0} −${file.deleted||0} · side by side op brede schermen, inline op smalle`:'Selecteer een bestand';const root=document.getElementById('job-change-diff');root.innerHTML=file?renderDiff(file,comments):'<div class="diff-empty">Selecteer links een bestand om de diff te bekijken.</div>';root.onmouseup=jobChangesState.bundle?.annotatable?captureCodeReviewSelection:null;
}
function cancelCodeReviewComment(){jobChangesState.selection=null;const form=document.getElementById('code-review-comment-form');if(!form)return;form.hidden=true;form.reset();window.getSelection()?.removeAllRanges();}
function reviewSelectionCell(node){const element=node?.nodeType===Node.TEXT_NODE?node.parentElement:node;return element?.closest?.('[data-review-side][data-review-line]')||null;}
function captureCodeReviewSelection(event){
  if(!jobChangesState.bundle?.annotatable)return;const root=document.getElementById('job-change-diff'),selection=window.getSelection();if(!selection)return;
  if(selection.isCollapsed){const cell=reviewSelectionCell(event?.target),code=cell?.querySelector('code');if(!cell||!code)return;const range=document.createRange();range.selectNodeContents(code);selection.removeAllRanges();selection.addRange(range);}
  if(selection.rangeCount!==1)return;const range=selection.getRangeAt(0),start=reviewSelectionCell(range.startContainer),end=reviewSelectionCell(range.endContainer);if(!start||!end||!root.contains(start)||!root.contains(end)||start.dataset.reviewSide!==end.dataset.reviewSide)return;
  const selected=selection.toString();if(!selected.trim())return;const startLine=Math.min(Number(start.dataset.reviewLine),Number(end.dataset.reviewLine)),endLine=Math.max(Number(start.dataset.reviewLine),Number(end.dataset.reviewLine)),file=jobChangesState.changes.files.find(candidate=>workspacePath(candidate.path)===jobChangesState.selectedPath);jobChangesState.selection={path:file?.path||jobChangesState.selectedPath,side:start.dataset.reviewSide,start_line:startLine,end_line:endLine,selected_text:selected};
  const location=document.getElementById('code-review-comment-location');location.textContent=`${shortPath(jobChangesState.selectedPath,40)}:${startLine}${endLine>startLine?`-${endLine}`:''} · ${start.dataset.reviewSide}`;location.title=jobChangesState.selectedPath;document.getElementById('code-review-comment-quote').textContent=selected;document.getElementById('code-review-comment-form').hidden=false;requestAnimationFrame(()=>document.getElementById('code-review-comment-body').focus());
}
function focusCodeReviewComment(id){
  const comment=(jobChangesState.bundle?.comments||[]).find(item=>item.id===id);if(!comment)return;openJobChangeDiff(comment.path);document.querySelectorAll('.comment-card.active,[data-code-comment-ids].active').forEach(element=>element.classList.remove('active'));document.querySelector(`[data-focus-code-comment="${CSS.escape(id)}"]`)?.classList.add('active');const marks=[...document.querySelectorAll('[data-code-comment-ids]')].filter(mark=>String(mark.dataset.codeCommentIds||'').split(' ').includes(id)&&mark.offsetParent!==null);marks.forEach(mark=>mark.classList.add('active'));marks[0]?.scrollIntoView({behavior:'smooth',block:'center'});
}
function setGateBusy(busy){const spinner=document.getElementById('auth-busy');if(spinner)spinner.hidden=!busy;}
function showAuthGate(copy,mode='login'){
  document.getElementById('app-shell').hidden=true;
  document.getElementById('auth-gate').hidden=false;
  document.getElementById('auth-copy').textContent=copy;setGateBusy(false);
  document.getElementById('setup-form').hidden=mode!=='setup';
  document.getElementById('login-form').hidden=mode!=='login';
}
function enterApp(status){
  authState=status;csrfToken=status.csrf_token||'';
  document.getElementById('auth-gate').hidden=true;document.getElementById('app-shell').hidden=false;
  document.getElementById('current-user').textContent=`${status.user.display_name||status.user.username} · ${status.user.role}`;
  document.getElementById('job-owner').value=status.user.username;
  document.getElementById('job-owner').readOnly=true;
  connectStateStream();
}
// A banner shows where the person is: inside the open dialog (a modal
// covers the page) or at the top of the page.
function showBanner(text,kind){const dialog=document.querySelector('dialog[open]');let box=document.getElementById('error');if(dialog){box=dialog.querySelector('.dialog-banner');if(!box){box=document.createElement('div');box.className='error-banner dialog-banner';dialog.prepend(box);}}box.textContent=text;box.classList.toggle('notice',kind==='notice');box.style.display='block';clearTimeout(box._hide);box._hide=setTimeout(()=>box.style.display='none',kind==='notice'?4500:6500);}
function showError(error){showBanner(error.message||error,'error');}
function showNotice(text){showBanner(text,'notice');}
// One vocabulary for everything that starts on a runner: a recording, a
// Session's capsule, the chat waiting for it. The stage names come from the
// runner; the words are here, once.
const startStages={prepare:'voorbereiden',parents:'basisimage naar runner',load:'image laden op de runner',start:'capsule starten',done:'klaar'};
function progressText(progress){if(!progress)return '';const stage=startStages[progress.stage]||progress.stage||'starten',bytes=progressPercent(progress)===null?'':` · ${formatBytes(progress.current||0)} / ${formatBytes(progress.total)} (${progressPercent(progress)}%)`;return `${stage}${bytes}${progress.message&&!progress.total?` · ${progress.message}`:''}`;}
function startProgressText(start){return `Starten · ${progressText(start)||'starten'}`;}
// progressPercent is the whole-number percentage of a progress with a total, else null.
function progressPercent(progress){return progress?.total?Math.floor((progress.current||0)/progress.total*100):null;}
// preparationText says where a Session's capsule stands: failed and
// retrying, being prepared on a runner, or still waiting for one.
function preparationText(preparing,session){
  if(preparing?.failure){const since=preparing.failure.at?elapsedSince(preparing.failure.at):'';return {text:`Starten mislukt · ${preparing.failure.error} · probeert opnieuw${since?` · ${since} geleden`:''}`,failed:true};}
  if(preparing){const client=byID(snapshot.clients,preparing.client_id||session?.client_id),since=preparing.started_at?elapsedSince(preparing.started_at):'',doing=progressText(preparing.progress);return {text:`${client?`Voorbereiden op ${client.name}`:'Runner zoeken'}${doing?` · ${doing}`:''}${since?` · ${since}`:''}`,failed:false};}
  return {text:session?.status==='queued'?'In de wachtrij · wacht op een runner':'Nog geen runner gekoppeld',failed:false};
}
// followingStartID guards the poller: one per recording, whether the browser
// issued the command or found the recording starting after a reload.
let followingStartID='';
// startState mirrors the start job the browser is following, for the recorder card.
let startState=null;
async function followStart(start){
  if(followingStartID===start.recording_id)return;
  followingStartID=start.recording_id;
  startState=start;renderRecording();
  try{
  for(let failures=0;;){
    await new Promise(resolve=>setTimeout(resolve,1500));
    try{
      const next=await api(`/api/recordings/${encodeURIComponent(start.recording_id)}/start`);failures=0;startState=next;renderRecording();
      if(next.status==='done'){startState=null;await refresh(true);return;}
      if(next.status==='cancelled'){startState=null;await refresh(true);return;}
      if(next.status==='error'){startState=null;showError(new Error(next.error||'Starten mislukt'));await refresh(true);return;}
    }catch(error){
      failures+=1;
      // 404: the server holds no job for this recording (an older server, or
      // the recording just ended); refresh so the card follows the truth.
      if(error.status===404||failures>=8){startState=null;showError(new Error(`Voortgang van het starten is niet meer op te vragen: ${error.message||error}. Gebruik Cancel om de opname op te ruimen.`));await refresh(true);return;}
    }
  }
  }finally{followingStartID='';}
}
// sealState mirrors the End & save job the browser is following, for the
// recorder panel; the console shows the same numbers as a progress line.
let sealState=null;
// Saving a layer speaks the same way as starting one: stage, bytes, percentage.
const sealStages={commit:'committen',archive:'archiveren',finish:'vastleggen',done:'klaar'};
function sealProgressText(seal){return `Opslaan · ${progressText({...seal,stage:sealStages[seal.stage]||seal.stage||'opslaan'})||'opslaan'}`;}
async function followSeal(seal){
  sealState=seal;renderRecording();
  for(let failures=0;;){
    await new Promise(resolve=>setTimeout(resolve,1500));
    try{
      const next=await api(`/api/recordings/${encodeURIComponent(seal.recording_id)}/seal`);failures=0;sealState=next;renderRecording();
      if(next.status==='done'){sealState=null;await refresh(true);return;}
      if(next.status==='error'){sealState=null;showError(new Error(next.error||'Opslaan mislukt'));await refresh(true);return;}
    }catch(error){
      failures+=1;if(error.status===404||failures>=8){sealState=null;showError(new Error(`Voortgang van het opslaan is niet meer op te vragen: ${error.message||error}`));await refresh(true);return;}
    }
  }
}
function setTab(name){
  name=({git:'connections',mcp:'connections',snapshots:'environments'}[name]||name);
  if(!document.getElementById(`tab-${name}`))name='jobs';
  document.querySelectorAll('.tab-button').forEach(button=>button.classList.toggle('active',button.dataset.tab===name));
  document.querySelectorAll('.tab-page').forEach(page=>page.classList.toggle('active',page.id===`tab-${name}`));
  document.querySelector('main.workspace')?.classList.toggle('explore',name==='explore');
  localStorage.setItem('spin-tab',name);
}
function setConnection(name){
  if(!document.getElementById(`connection-${name}`))name='git';
  document.querySelectorAll('[data-connection]').forEach(button=>button.classList.toggle('active',button.dataset.connection===name));
  document.querySelectorAll('.connection-page').forEach(page=>page.classList.toggle('active',page.id===`connection-${name}`));
  localStorage.setItem('spin-connection',name);
}
function setWorkView(name){if(!document.getElementById(`work-${name}`))name='jobs';document.querySelectorAll('[data-work-view]').forEach(button=>button.classList.toggle('active',button.dataset.workView===name));document.querySelectorAll('.work-page').forEach(page=>page.classList.toggle('active',page.id===`work-${name}`));localStorage.setItem('spin-work-view',name);}
function jobIsClosed(job){return job.status==='done'||job.status==='cancelled'||job.workflow_status==='done';}
function jobAssignee(job){return job.assignee||job.owner||'';}
function setJobState(name){jobStateFilter=['mine','all','closed'].includes(name)?name:'mine';document.querySelectorAll('[data-job-state]').forEach(button=>button.classList.toggle('active',button.dataset.jobState===jobStateFilter));const copy={mine:['Mijn werk','Jobs die bij jou liggen: van jou, of aan jou toegewezen om naar te kijken.'],all:['Actief werk','Alle open Jobs van het team; wijs een Job toe om hem bij iemand neer te leggen.'],closed:['Afgerond werk','Afgeronde en handmatig gesloten Jobs blijven volledig terugleesbaar.']}[jobStateFilter];document.getElementById('jobs-list-title').textContent=copy[0];document.getElementById('jobs-list-copy').textContent=copy[1];localStorage.setItem('spin-job-state',jobStateFilter);document.getElementById('job-search-row').hidden=jobStateFilter!=='closed';renderJobs();}
function activeRecording(){return snapshot.recordings.find(recording=>recording.actor===currentOperator()&&recording.status==='recording');}
// PTY sessions render in xterm.js: a real terminal in the browser, with raw
// keystrokes, colours, cursor movement and resizing, so login flows and
// TUIs work. The line console above it stays as the log of what was done.
const terminalTheme={background:'#050806',foreground:'#dfe7e2',cursor:'#7be4ad',cursorAccent:'#050806',selectionBackground:'#2a4a38',black:'#0b0f0c',brightBlack:'#56635c',green:'#7be4ad',brightGreen:'#9ff0c4',blue:'#8fb8e8',brightBlue:'#a9c3df',yellow:'#e8c77b',brightYellow:'#f0d69b',red:'#f08a8a',brightRed:'#f5a0a0'};
function activeTerminal(){return activeTerminalID?terminalSessions.get(activeTerminalID):null;}
function liveTerminals(){return [...terminalSessions.values()].filter(session=>!session.exited);}
function ptyStage(){return document.getElementById('pty-stage');}
function fitTerminal(session){if(!session?.term||session.pane.hidden||ptyStage().hidden)return;try{session.fit.fit();}catch(_){}}
function showTerminalPane(session){const stage=ptyStage();stage.querySelectorAll('.pty-pane').forEach(pane=>pane.hidden=pane!==session.pane);updateStage();requestAnimationFrame(()=>{fitTerminal(session);session.term.focus();});}
function updateStage(){const any=terminalSessions.size>0,empty=document.getElementById('pty-empty');empty.hidden=any;if(!any)empty.textContent=terminalTarget()?'Shell wordt geopend…':(activeRecording()?'De capsule van je opname start; de shell volgt zodra hij er is.':'Geen draaiende capsule. Start een opname, of Start een laag onder Environments.');}
function chooseTerminal(id){const session=terminalSessions.get(id);if(!session)return;activeTerminalID=id;updateTerminalControls();showTerminalPane(session);}
function closeTerminalPane(id){const session=terminalSessions.get(id);if(!session)return;if(!session.exited){session.exited=true;try{session.socket.close();}catch(_){}}terminalSessions.delete(id);try{session.term.dispose();}catch(_){}session.pane.remove();if(activeTerminalID===id)activeTerminalID=[...terminalSessions.keys()].at(-1)||null;updateStage();updateTerminalControls();const next=activeTerminal();if(next)showTerminalPane(next);}
function updateTerminalControls(){
  updateStage();
  const selected=activeTerminal(),live=liveTerminals().length,channels=document.getElementById('terminal-channels'),recording=activeRecording(),composition=activeComposition(),target=recording||composition;
  document.getElementById('terminal-interrupt').hidden=!selected||selected.exited;
  channels.innerHTML=[...terminalSessions.values()].map(session=>`<button class="channel ${session.id===activeTerminalID?'active':''} ${session.exited?'exited':''}" data-terminal-id="${esc(session.id)}">${esc(session.label)} · ${esc(session.title.slice(0,28))}${session.exited?` · exit ${esc(String(session.exitCode??'?'))}`:''}<span class="channel-close" data-close-terminal="${esc(session.id)}" title="Sluiten">✕</span></button>`).join('')+(target?'<button class="channel" id="new-terminal">+ shell</button>':'');
  channels.querySelectorAll('[data-terminal-id]').forEach(button=>button.onclick=()=>chooseTerminal(button.dataset.terminalId));
  channels.querySelectorAll('[data-close-terminal]').forEach(button=>button.onclick=event=>{event.stopPropagation();closeTerminalPane(button.dataset.closeTerminal);});
  const add=channels.querySelector('#new-terminal');if(add)add.onclick=()=>{const target=terminalTarget();if(target)startTerminalCommand(target,shellCommand,{title:'shell'});};
  const status=document.getElementById('terminal-status');
  if(live){status.className='terminal-status recording';status.innerHTML=`<span class="rec-dot"></span><span>${live} shell${live===1?'':'s'}</span>`;}
  else if(recording){status.className='terminal-status recording';status.innerHTML=`<span class="rec-dot"></span><span>REC ${esc(recording.kind)}:${esc(recording.name)}</span>`;}
  else if(composition){status.className='terminal-status';status.innerHTML=`<span class="rec-dot"></span><span>${esc(composition.selector)} draait</span>`;}
}
function finishTerminal(session,exitCode,refreshState=true){
  if(!terminalSessions.has(session.id)||session.exited)return;
  session.exited=true;session.exitCode=exitCode;
  try{session.socket.close();}catch(_){}
  updateTerminalControls();
  if(refreshState)refresh(true);
}
// The terminal is a shell by default: as soon as a capsule is ready (the
// open recording, or the started layer) one is opened in it, and every
// line that is not a Spin command goes there. Extra shells come from + PTY.
const shellCommand='if command -v bash >/dev/null 2>&1; then exec bash -il; else exec sh -il; fi';
let shellAutoTarget='';
function terminalTarget(){const recording=activeRecording();if(recording)return recording.runtime?.container_id?{...recording,kind:'recording'}:null;return activeComposition();}
function ensureShell(){
  const dialog=document.getElementById('capsule-dialog'),target=terminalTarget();
  if(!dialog?.open||!target)return;
  if(liveTerminals().some(session=>session.targetID===target.id)||shellAutoTarget===target.id)return;
  shellAutoTarget=target.id;startTerminalCommand(target,shellCommand,{title:'shell'});
}
function focusTerminal(){const session=activeTerminal();if(session&&!session.exited)session.term.focus();}
// startTerminalCommand opens a PTY on the given target: the open recording
// (recorded into the layer) or the operator's started layer (not recorded).
function startTerminalCommand(target,line,options={}){
  if(liveTerminals().length>=8){showError(new Error('Deze capsule heeft al acht shells.'));return;}
  const id=`terminal-${++terminalSequence}`,label=`T${terminalSequence}`,title=options.title||line,path=target.kind==='composition'?`/api/compositions/${encodeURIComponent(target.id)}/terminal`:`/api/recordings/${encodeURIComponent(target.id)}/terminal`;
  const protocol=location.protocol==='https:'?'wss:':'ws:',socket=new WebSocket(`${protocol}//${location.host}${path}`);
  const pane=document.createElement('div');pane.className='pty-pane';ptyStage().appendChild(pane);
  const mono=(getComputedStyle(document.documentElement).getPropertyValue('--mono')||'').trim()||'ui-monospace, Menlo, monospace';
  const term=new Terminal({cursorBlink:true,fontFamily:mono,fontSize:12.5,lineHeight:1.15,scrollback:5000,theme:terminalTheme,convertEol:false});
  const fit=new FitAddon.FitAddon();term.loadAddon(fit);term.open(pane);
  const session={id,label,socket,targetID:target.id,line,title,exited:false,exitCode:null,pane,term,fit,ready:false};
  terminalSessions.set(id,session);activeTerminalID=id;updateTerminalControls();showTerminalPane(session);
  const send=message=>{if(socket.readyState===WebSocket.OPEN&&!session.exited)socket.send(JSON.stringify(message));};
  term.onData(data=>send({type:'input',data}));
  term.onResize(({rows,cols})=>{if(session.ready)send({type:'resize',rows,cols});});
  socket.onopen=()=>{fitTerminal(session);socket.send(JSON.stringify({type:'start',command:line,rows:term.rows,cols:term.cols}));};
  socket.onmessage=event=>{let message;try{message=JSON.parse(event.data);}catch(_){return;}
    if(message.type==='ready'){session.ready=true;term.focus();if(options.initial)send({type:'input',data:String(options.initial)+'\r'});return;}
    if(message.type==='output'){term.write(message.data);return;}
    if(message.type==='error'){term.write(`\r\n\x1b[31m${message.error||'Interactieve terminalfout'}\x1b[0m\r\n`);finishTerminal(session,null);return;}
    if(message.type==='exit'){term.write(`\r\n\x1b[2m[exit ${message.exit_code}]\x1b[0m\r\n`);finishTerminal(session,message.exit_code);}
  };
  socket.onerror=()=>{if(!session.exited)term.write('\r\n\x1b[31mKon de shell niet openen.\x1b[0m\r\n');};
  socket.onclose=()=>{if(!session.exited){term.write('\r\n\x1b[2m[verbinding gesloten]\x1b[0m\r\n');finishTerminal(session,null);}};
}
function interruptTerminal(){const session=activeTerminal();if(session&&!session.exited&&session.socket.readyState===WebSocket.OPEN)session.socket.send(JSON.stringify({type:'interrupt'}));}
// activeComposition is the operator's own started layer with a running
// capsule: a place to look around without recording.
function activeComposition(){const composition=snapshot.compositions.find(item=>item.operator===currentOperator()&&!item.session_id&&item.runtime?.status==='ready');return composition?{...composition,kind:'composition'}:null;}

// Actions on layers and capsules go straight to the API; the cards show the
// result and the runner's progress, errors are toasts.
function markStarting(label){const status=document.getElementById('terminal-status');status.className='terminal-status recording';status.innerHTML=`<span class="rec-dot"></span><span>start ${esc(label)}</span>`;}
async function runAction(request,onDone){
  try{const response=await request();if(onDone)await onDone(response);await refresh(true);}
  catch(error){sealState=null;startState=null;showError(error);await refresh(true).catch(()=>{});}
}
// A recording answers with the recording when the capsule is up, or with the
// start job to follow when it takes a moment.
async function recordingStarted(response){
  if(response.recording_id&&response.status){await refresh(true);await followStart(response);return;}
  startState=null;
}
function startLayerRecording(payload){
  const label=`${payload.kind}:${payload.name}`;markStarting(label);
  return runAction(()=>api('/api/recordings',{method:'POST',body:JSON.stringify({...payload,actor:currentOperator()})}),recordingStarted);
}
function editLayer(artifact){
  const label=artifactSelector(artifact);markStarting(`Nieuwe versie van ${label}`);
  return runAction(()=>api(`/api/artifacts/${encodeURIComponent(artifact.id)}/edit`,{method:'POST',body:JSON.stringify({operator:currentOperator()})}),recordingStarted);
}
function endRecording(recording){
  sealState={recording_id:recording.id,status:'running',stage:'commit',message:'Capsule wordt gecommit op de runner'};renderRecording();
  return runAction(()=>api(`/api/recordings/${encodeURIComponent(recording.id)}/end`,{method:'POST',body:JSON.stringify({actor:currentOperator()})}),async response=>{
    if(response.recording_id&&response.status){await refresh(true);await followSeal(response);return;}
    sealState=null;
  });
}
function cancelRecording(recording){return runAction(()=>api(`/api/recordings/${encodeURIComponent(recording.id)}/cancel`,{method:'POST',body:JSON.stringify({actor:currentOperator()})}),()=>{sealState=null;startState=null;});}
function useLayers(selector,withSelectors=[]){return runAction(()=>api('/api/use',{method:'POST',body:JSON.stringify({operator:currentOperator(),selector,with_selectors:withSelectors,profile:'default'})}),composition=>{if(composition.warnings?.length)showError(new Error(composition.warnings.join('; ')));});}
function useSession(sessionID){return runAction(()=>api('/api/use',{method:'POST',body:JSON.stringify({operator:currentOperator(),session_id:sessionID})}));}
function stopComposition(id){return runAction(()=>api(`/api/compositions/${encodeURIComponent(id)}/stop`,{method:'POST',body:JSON.stringify({operator:currentOperator()})}));}
function probeACP(id){return runAction(()=>api(`/api/compositions/${encodeURIComponent(id)}/acp/probe`,{method:'POST',body:JSON.stringify({operator:currentOperator()})}),probe=>alert(`ACP v1 handshake geslaagd · ${probe.enablement?.command||''}\n\n${JSON.stringify(probe.handshake,null,2)}`));}
// bindActionButtons wires data-action buttons to these actions.
function bindActionButtons(root=document){root.querySelectorAll('[data-action]').forEach(button=>button.onclick=()=>{
  const {action,id,selector}=button.dataset,recording=activeRecording();
  setTab('environments');openConsole();
  switch(action){
    case 'end':if(recording)endRecording(recording);break;
    case 'cancel':if(recording)cancelRecording(recording);break;
    case 'use':useLayers(selector);break;
    case 'use-session':useSession(id);break;
    case 'stop':stopComposition(id);break;
    case 'probe':probeACP(id);break;
  }
});}

// Rendering is idempotent and full, but the DOM is only touched where the
// HTML differs, and never where the user is: a region holding the focused
// control (an open dropdown, a field being typed in) keeps its DOM until
// focus leaves, then gets what it missed.
function regionBusy(root){const active=document.activeElement;return Boolean(active&&active!==document.body&&root.contains(active)&&active.matches('select,input,textarea,[contenteditable="true"]'));}
function deferRegion(root,apply){root._pendingRender=apply;if(root._pendingBound)return;root._pendingBound=true;root.addEventListener('focusout',()=>setTimeout(()=>{if(root._pendingRender&&!regionBusy(root)){const apply=root._pendingRender;root._pendingRender=null;apply();}},0));}
// patchRegion writes a region's HTML only when it changed, and not while the
// user is in it; a deferred write lands on focusout and is followed by a
// full render so handlers are bound again.
const innerHTMLDescriptor=Object.getOwnPropertyDescriptor(Element.prototype,'innerHTML');
function patchRegion(root,html){
  if(root._renderedHTML===html)return false;
  const write=()=>{innerHTMLDescriptor.set.call(root,html);root._renderedHTML=html;};
  if(regionBusy(root)){deferRegion(root,()=>{if(root._renderedHTML!==html){write();render();}});return false;}
  write();return true;
}
// region returns a container whose innerHTML assignments go through
// patchRegion: renderers keep writing full HTML, the DOM only moves when the
// HTML does. Handlers are (re)bound after every render; that is idempotent.
function region(id){const root=document.getElementById(id);if(root&&!root._region){root._region=true;Object.defineProperty(root,'innerHTML',{get(){return innerHTMLDescriptor.get.call(root);},set(value){patchRegion(root,String(value));}});}return root;}
// patchKeyed keeps every child whose HTML is unchanged, replaces the ones
// that changed, and reorders/removes/adds by key; a child the user is in
// waits for focus to leave. Returns whether anything changed.
function patchKeyed(root,entries){
  const current=new Map([...root.children].filter(node=>node.dataset.key).map(node=>[node.dataset.key,node]));let changed=false;
  const nodes=entries.map(({key,html})=>{const existing=current.get(key);if(existing&&existing._renderedHTML===html)return existing;
    if(existing&&regionBusy(existing)){deferRegion(existing,()=>{if(existing._renderedHTML!==html){const fresh=document.createElement('template');fresh.innerHTML=html;const node=fresh.content.firstElementChild;node.dataset.key=key;node._renderedHTML=html;existing.replaceWith(node);root._afterPatch?.();}});return existing;}
    const template=document.createElement('template');template.innerHTML=html;const node=template.content.firstElementChild;node.dataset.key=key;node._renderedHTML=html;changed=true;return node;});
  if(!changed&&nodes.length===root.children.length&&nodes.every((node,index)=>root.children[index]===node))return false;
  root.replaceChildren(...nodes);return true;
}
function render(){
  // A re-render must not move the page: remember where it was and put it back.
  const scrollY=window.scrollY,scrolled=[...document.querySelectorAll('.workspace,.panel-body,.artifacts,.jobs')].map(node=>[node,node.scrollTop]);
  try{renderAll();}finally{if(Math.abs(window.scrollY-scrollY)>1)window.scrollTo(0,scrollY);scrolled.forEach(([node,top])=>{if(node.isConnected&&Math.abs(node.scrollTop-top)>1)node.scrollTop=top;});}
}
function renderAll(){
  if(chatState.sessionID&&document.getElementById('chat-dialog').open&&!workflowSessionIsActive(chatState.sessionID))closeDialog('chat-dialog');
  const onlineClients=snapshot.clients.filter(client=>client.status==='online').length,connections=snapshot.git_repositories.length+snapshot.git_accounts.length+myMCP().length+onlineClients;
  const counts={jobs:`(${snapshot.jobs.length}/${snapshot.sessions.length})`,explore:`(${snapshot.git_repositories.length})`,environments:`(${snapshot.artifacts.length})`,connections:`(${connections})`,access:`(${snapshot.users.length})`};
  Object.entries(counts).forEach(([name,value])=>document.getElementById(`nav-count-${name}`).textContent=value);
  document.getElementById('runner-segment-count').textContent=`${onlineClients}/${snapshot.clients.length}`;
  const descriptions={jobs:`${snapshot.jobs.length} Jobs, ${snapshot.sessions.length} Sessions`,environments:`${snapshot.artifacts.length} lagen`,connections:`${snapshot.git_repositories.length} repositories, ${snapshot.git_accounts.length} Git-identities, ${myMCP().length} MCP-configuraties, ${onlineClients} runners online`,access:`${snapshot.users.length} gebruikers`};
  Object.entries(descriptions).forEach(([name,value])=>{const button=document.querySelector(`[data-tab="${name}"]`);button.title=value;button.setAttribute('aria-label',value);});
  renderRecording(); renderComposition(); renderArtifacts(); renderGitOptions(); renderExploreOptions(); renderTemplateOptions(); renderEnvironmentOptions(); renderMCPOptions(); renderJobs(); renderTemplates(); renderGitAccounts(); renderGit(); renderMCP(); renderRunners(); renderAccess();
}

function renderRecording(){
  ensureShell();
  const recording=activeRecording(),status=region('terminal-status'),root=document.getElementById('recording-section');
  renderRecordFromOptions();
  if(!recording){status.className='terminal-status';status.innerHTML='<span class="rec-dot"></span><span>idle</span>';root.innerHTML=`<div class="recorder-idle"><p>${activeComposition()?`Shell van <strong>${esc(activeComposition().selector)}</strong>. Er wordt niets opgenomen.`:'Geen opname. Een nieuwe laag begint met een opname in een verse capsule.'}</p><button class="primary" type="button" id="recorder-new-layer">${icon('fiber_manual_record')}Nieuwe laag</button></div>`;root.querySelector('#recorder-new-layer').onclick=()=>openLayerDialog({scope:'user'});updateTerminalControls();return;}
  if(!terminalSessions.size){status.className='terminal-status recording';status.innerHTML=`<span class="rec-dot"></span><span>REC ${esc(recording.kind)}:${esc(recording.name)}</span>`;}
  // A recording without a capsule is being started by a job on the server;
  // attach to that job when this browser is not following it yet (after a
  // reload, or when another tab gave the command).
  const starting=startState&&startState.recording_id===recording.id&&startState.status==='running'?startState:(recording.runtime?.container_id?null:{stage:'prepare',message:'Voortgang ophalen'});
  if(!recording.runtime?.container_id&&followingStartID!==recording.id)followStart({recording_id:recording.id,status:'running',stage:'prepare',message:'Voortgang ophalen'});
  const runtime=recording.runtime?.container_id?'<small>Werk in de shell hieronder; + shell opent een tweede in dezelfde capsule.</small>':`<div class="seal-progress"><div class="seal-track ${progressPercent(starting)===null?'indeterminate':''}"><div class="seal-fill" style="width:${progressPercent(starting)??100}%"></div></div><small>${esc(startProgressText(starting||{}))}</small></div>`;
  if(!recording.runtime?.container_id){status.className='terminal-status recording';status.innerHTML=`<span class="rec-dot"></span><span>STARTING ${esc(recording.kind)}:${esc(recording.name)}${progressPercent(starting)===null?'':` ${progressPercent(starting)}%`}</span>`;}
  const sealing=sealState&&sealState.recording_id===recording.id&&sealState.status==='running'?sealState:null;
  // While the capsule is still starting the runtime block already carries the
  // progress bar; there is nothing to end yet, but the start can be cancelled.
  const actions=sealing?`<div class="seal-progress"><div class="seal-track ${progressPercent(sealing)===null?'indeterminate':''}"><div class="seal-fill" style="width:${progressPercent(sealing)??100}%"></div></div><small>${esc(sealProgressText(sealing))}</small></div>`:(recording.runtime?.container_id?`<div class="panel-actions" style="margin-top:9px"><button class="small-button" data-action="end">End & save</button><button class="danger" data-action="cancel">Cancel</button></div>`:`<div class="panel-actions" style="margin-top:9px"><button class="danger" data-action="cancel">Cancel start</button></div>`);
  if(sealing){status.className='terminal-status recording';status.innerHTML=`<span class="rec-dot"></span><span>SAVING ${esc(recording.kind)}:${esc(recording.name)}${progressPercent(sealing)===null?'':` ${progressPercent(sealing)}%`}</span>`;}
  root.innerHTML=`<h3>Recorder</h3><div class="record-card"><strong>● ${esc(recording.kind)}:${esc(recording.name)}</strong><small>${esc(recording.scope)} · ${(recording.commands||[]).length} commands</small>${recording.enables?.length?`<small>ENABLES <span class="capability">${esc(enabledNames(recording.enables))}</span></small>`:''}${runtime}${actions}</div>`;
  bindActionButtons(root);updateTerminalControls();
}

// A layer's manifest: its real difference by kind, and what sealing left
// out because it was identical to the layer below or a cache.
function contentsLine(contents){
  if(!contents||!contents.files&&!contents.dropped_identical?.files)return '';
  const dropped=contents.dropped_identical?.files?` · weggelaten: ${esc(formatBytes(contents.dropped_identical.bytes))} gelijk aan de laag eronder`:'';
  return `<small>${icon('inventory_2')} ${contents.files} bestand${contents.files===1?'':'en'} · ${esc(formatBytes(contents.bytes))}${dropped}</small>`;
}
function contentsButton(artifact){const tracked=(artifact.tracked_paths||[]).length;return artifact.snapshot?.contents?`<button class="small-button" data-contents-artifact="${esc(artifact.id)}" title="Alle paden van deze laag, per soort; vink aan wat Spin tussen Sessions bijhoudt">${icon('inventory_2')}Inhoud${tracked?` · ${tracked} bijgehouden`:''}</button>`:'';}
// The contents dialog lists every path of a layer's manifest or a capsule's
// changes, largest first, filtered by text and kind.
const contentsState={entries:[],title:'',artifactID:'',tracked:new Set()};
async function openContents(url,title,artifact=null){
  try{
    const result=await api(url),contents=result.contents||{},entries=result.entries||[];
    contentsState.entries=entries;contentsState.title=title;contentsState.artifactID=artifact?artifact.id:'';
    contentsState.tracked=new Set(artifact?(artifact.tracked_paths||[]):[]);contentsOpenFolders.clear();
    document.getElementById('contents-save').hidden=!artifact;document.getElementById('contents-save').disabled=false;
    document.getElementById('contents-title').textContent=title;
    const dropped=contents.dropped_identical?.files?` · weggelaten: ${formatBytes(contents.dropped_identical.bytes)} in ${contents.dropped_identical.files} bestand${contents.dropped_identical.files===1?'':'en'} gelijk aan de laag eronder`:'';
    document.getElementById('contents-summary').textContent=`${contents.files||0} bestanden · ${formatBytes(contents.bytes||0)}${dropped}${entries.length?'':' · geen lijst bewaard voor deze laag'}`;
    document.getElementById('contents-filter').value='';
    renderContentsList();openDialog('contents-dialog');
  }catch(error){showError(error);}
}
const contentsOpenFolders=new Set();
function contentsTreeModel(entries){const root={dirs:new Map(),files:[]};entries.forEach(entry=>{const parts=entry.path.replace(/^\//,'').split('/');let node=root;parts.slice(0,-1).forEach(part=>{if(!node.dirs.has(part))node.dirs.set(part,{dirs:new Map(),files:[]});node=node.dirs.get(part);});node.files.push({name:parts.at(-1),path:entry.path,size:entry.bytes});});return root;}
function contentsFilesBelow(node,out=[]){node.files.forEach(file=>out.push(file));node.dirs.forEach(child=>contentsFilesBelow(child,out));return out;}
// Folders render their children when they open: a tool layer holds tens
// of thousands of files, and drawing them all at once would freeze the page.
function contentsFolderNode(path){let node=contentsState.model;for(const part of path.split('/')){node=node?.dirs.get(part);if(!node)return null;}return node;}
function renderContentsNode(node,depth,prefix,track,openAll){
  const dirs=[...node.dirs.entries()].sort((a,b)=>a[0].localeCompare(b[0])),files=node.files.sort((a,b)=>a.name.localeCompare(b.name));
  return dirs.map(([name,child])=>{const path=prefix?`${prefix}/${name}`:name,open=openAll||contentsOpenFolders.has(path),below=contentsFilesBelow(child),bytes=below.reduce((sum,file)=>sum+(file.size||0),0),all=below.length>0&&below.every(file=>contentsState.tracked.has(file.path)),some=below.some(file=>contentsState.tracked.has(file.path));
    return `<div class="tree-folder${open?' open':''}"><div class="tree-row">${track?`<input type="checkbox" data-track-folder="${esc(path)}" ${all?'checked':''} ${!all&&some?'data-indeterminate="1"':''} title="Alles in deze map bijhouden">`:''}<button type="button" class="tree-row-toggle" data-toggle-contents-folder="${esc(path)}" title="${esc('/'+path)}"><span class="material-symbols-outlined tree-slot" aria-hidden="true">chevron_right</span><span class="material-symbols-outlined tree-icon folder" aria-hidden="true">${open?'folder_open':'folder'}</span><span class="tree-name">${esc(name)}</span></button><span class="tree-size">${below.length} · ${esc(formatBytes(bytes))}</span></div><div class="tree-children" ${open?'':'hidden'}>${open?renderContentsNode(child,depth+1,path,track,openAll):''}</div></div>`;}).join('')+
    files.slice(0,1500).map(file=>`<div class="tree-row tree-file">${track?`<input type="checkbox" data-track-path="${esc(file.path)}" ${contentsState.tracked.has(file.path)?'checked':''}>`:''}<span class="tree-slot"></span><span class="material-symbols-outlined tree-icon" aria-hidden="true">${fileIcon(file.name)}</span><span class="tree-name" title="${esc(file.path)}">${esc(file.name)}</span><span class="tree-size">${esc(formatBytes(file.size||0))}</span></div>`).join('')+(files.length>1500?`<div class="tree-row tree-file"><span class="tree-slot"></span><span class="tree-name hint">nog ${files.length-1500} bestanden in deze map; filter op pad om ze te zien</span></div>`:'');
}
function bindContentsRows(root){
  root.querySelectorAll('[data-indeterminate]').forEach(box=>box.indeterminate=true);
  root.querySelectorAll('[data-toggle-contents-folder]').forEach(button=>button.onclick=()=>{const path=button.dataset.toggleContentsFolder,folder=button.closest('.tree-folder'),children=folder.querySelector(':scope>.tree-children'),open=!folder.classList.contains('open');folder.classList.toggle('open',open);button.querySelector('.tree-icon').textContent=open?'folder_open':'folder';if(open){contentsOpenFolders.add(path);if(!children.innerHTML){const node=contentsFolderNode(path);if(node){children.innerHTML=renderContentsNode(node,path.split('/').length,path,Boolean(contentsState.artifactID),false);bindContentsRows(children);}}}else contentsOpenFolders.delete(path);children.hidden=!open;});
  root.querySelectorAll('[data-track-path]').forEach(box=>box.onchange=()=>{if(box.checked)contentsState.tracked.add(box.dataset.trackPath);else contentsState.tracked.delete(box.dataset.trackPath);});
  root.querySelectorAll('[data-track-folder]').forEach(box=>box.onchange=()=>{const prefix='/'+box.dataset.trackFolder+'/';contentsState.entries.filter(entry=>entry.path.startsWith(prefix)).forEach(entry=>{if(box.checked)contentsState.tracked.add(entry.path);else contentsState.tracked.delete(entry.path);});renderContentsList();});
}
function renderContentsList(){
  const filter=document.getElementById('contents-filter').value.trim().toLowerCase(),root=document.getElementById('contents-body');
  const rows=contentsState.entries.filter(entry=>!filter||entry.path.toLowerCase().includes(filter));
  const track=Boolean(contentsState.artifactID),openAll=Boolean(filter)&&rows.length<=1500;
  contentsState.model=contentsTreeModel(rows);
  root.innerHTML=rows.length?`${rows.length>1500&&filter?`<p class="hint">${rows.length} bestanden passen; mappen staan dicht, klap open wat je zoekt of verfijn het filter.</p>`:''}<div class="code-tree contents-tree">${renderContentsNode(contentsState.model,0,'',track,openAll)}</div>`:'<div class="empty">Niets gevonden.</div>';
  bindContentsRows(root);
}
document.getElementById('contents-save').onclick=async()=>{const button=document.getElementById('contents-save');button.disabled=true;try{await api(`/api/artifacts/${encodeURIComponent(contentsState.artifactID)}/tracked`,{method:'PUT',body:JSON.stringify({paths:[...contentsState.tracked]})});showNotice(`${contentsState.tracked.size} bestand${contentsState.tracked.size===1?'':'en'} bijgehouden voor ${contentsState.title}`);await refresh(true);}catch(error){showError(error);}finally{button.disabled=false;}};
document.getElementById('contents-filter').oninput=renderContentsList;
function renderComposition(){
  const composition=snapshot.compositions.find(item=>item.operator===currentOperator()),root=region('composition');
  if(!composition){root.innerHTML='<div class="empty">Nog geen draaiende Composition.</div>';return;}
  const bindings=Object.entries(composition.slot_bindings||{}).map(([slot,id])=>{const artifact=byID(snapshot.artifacts,id);return `<div class="binding"><span>${esc(slot)}</span><span>${esc(artifact?artifactSelector(artifact):id)}</span></div>`;}).join('');
  const enabled=composition.enabled?.length?`<div class="binding"><span>ENABLED</span><span>${esc(enabledNames(composition.enabled))}</span></div>`:'';
  const stack=(composition.layers||[]).map(id=>{const artifact=byID(snapshot.artifacts,id);return artifact?artifactSelector(artifact):id;});
  const withLayers=stack.length?`<div class="binding"><span>lagen</span><span title="Van onder naar boven">${stack.map(esc).join(' → ')}</span></div>`:'';
  const mcp=composition.mcp_server_ids?.length?`<div class="binding"><span>MCP</span><span>${composition.mcp_server_ids.map(id=>esc(byID(snapshot.mcp_servers,id)?.name||id)).join(', ')}</span></div>`:'';
  const git=composition.git?`<div class="binding"><span>GIT</span><span>${esc(composition.git.repository_name)} · ${esc(composition.git.head_ref)} → ${esc(composition.git.target_ref)} · ${esc(composition.git.credential_scope||'public')}${composition.git.login?' · '+esc(composition.git.provider+':'+composition.git.login):''}</span></div>`:'';
  const runtime=composition.runtime?`<div class="binding"><span>capsule</span><span>${esc(composition.runtime.status)} · ${esc(composition.runtime.attach_command||'')}</span></div>`:'';
  const changes=composition.capsule_changes?.files?`<div class="binding"><span>gewijzigd</span><span title="Buiten de workspace, sinds de start van de capsule">${composition.capsule_changes.files} bestand${composition.capsule_changes.files===1?'':'en'} · ${esc(formatBytes(composition.capsule_changes.bytes))} <button class="small-button" id="composition-changes-button">${icon('inventory_2')}Bekijk</button></span></div>`:'';
  const ready=composition.runtime&&composition.runtime.status!=='stopped';
  const acp=(composition.enabled||[]).some(item=>item.name==='acp');
  root.innerHTML=`<div class="meta"><span class="tag">${short(composition.id)}</span><span class="tag">${esc(composition.selector)}</span></div>${bindings}${withLayers}${enabled}${mcp}${git}${runtime}${changes}<div class="panel-actions" style="margin-top:9px">${ready&&acp?`<button class="small-button" data-action="probe" data-id="${esc(composition.id)}">ACP probe</button>`:''}${ready?`<button class="danger" data-action="stop" data-id="${esc(composition.id)}">Stop</button>`:''}</div>`;
  const changesButton=document.getElementById('composition-changes-button');if(changesButton)changesButton.onclick=()=>openContents(`/api/compositions/${encodeURIComponent(composition.id)}/changes`,'Gewijzigd in de capsule, buiten de workspace');
  bindActionButtons(root);
}

function acpLayerOf(artifact,seen=new Set()){if(!artifact||seen.has(artifact.id))return null;seen.add(artifact.id);if((artifact.enables||[]).some(item=>item.name==='acp'))return artifact;for(const parentID of artifact.parent_artifact_ids||[]){const found=acpLayerOf(byID(snapshot.artifacts,parentID),seen);if(found)return found;}return null;}
function renderArtifacts(){
  const root=region('artifacts'),artifacts=snapshot.artifacts.filter(canUse);
  if(!artifacts.length){root.innerHTML='<div class="empty">Nog geen environments. Open de recorder om <code>tool:git</code> als eerste laag te maken.</div>';return;}
  root.innerHTML=artifacts.map(artifact=>{
    const identity=artifact.subject?`${artifact.scope}:${artifact.subject}`:artifact.scope;
    const use=canUse(artifact)?`<button class="small-button" data-action="use" data-selector="${esc(artifactSelector(artifact))}">${icon('arrow_forward')}Start</button>`:'';
    const next=`<button class="small-button" data-record-from="${esc(artifactSelector(artifact))}">${icon('add')}Laag</button><button class="small-button" data-edit-artifact="${esc(artifactSelector(artifact))}" title="Neem deze laag opnieuw op met al haar instellingen en voeg toe wat ontbreekt; End &amp; save vervangt de huidige versie, ook voor lagen die erop gebouwd zijn">${icon('edit')}Bewerk</button>`;
    const remove=artifact.created_by===currentOperator()||authState.user?.role==='admin'?`<button class="danger" data-remove-artifact="${esc(artifact.id)}" data-artifact-label="${esc(artifactSelector(artifact))}">${icon('delete')}Verwijder</button>`:'';
    // The agent's options live on the layer that ENABLES acp; a credential
    // layer built on it shows and refreshes the same options, probed as the
    // operator's own identity.
    const agentLayer=acpLayerOf(artifact),acp=Boolean(agentLayer),options=agentLayer?.agent_options;
    // An ACP layer keeps what its agent offers (models, reasoning efforts)
    // once fetched, so a Template can choose per phase without starting it.
    const optionsLine=acp?`<small>Agent-opties · ${options?.fetching?`${icon('progress_activity')} ophalen… (capsule start, agent meldt zich) · `:''}${options?.error?`<span class="tag warning">ophalen mislukt · ${esc(options.error)}</span>`:options?`${(options.modes||[]).length} modes · ${(options.models||[]).length} modellen · ${(options.reasoning_efforts||[]).length} reasoning-niveaus · ${esc(options.agent_name||'agent')} · ${esc(new Date(options.fetched_at).toLocaleString())}${!(options.models||[]).length?' · deze agent biedt model en reasoning niet via ACP aan; zet ze in acp.env':''}`:'nog niet opgehaald'}</small>`:'';
    const fetchOptions=acp?`<button class="small-button" data-fetch-options="${esc(artifact.id)}" ${options?.fetching?'disabled':''} title="Start de agent één keer in een wegwerp-capsule (deze laag plus de lagen erboven, zoals je credential-laag) en bewaar welke modes, modellen en reasoning-niveaus hij aanbiedt">${icon(options?.fetching?'hourglass_top':'tune')}${options?.fetching?'Ophalen…':options&&!options.error?'Opties vernieuwen':'Opties ophalen'}</button>`:'';
    // The layer that ENABLES acp decides how its agent starts, chosen from
    // what the agent offered; the settings are metadata and survive a new version.
    const settings=agentLayer?.agent_settings||{},pick=(name,label,items,current,auto)=>`<label class="agent-setting"><span>${esc(label)}</span><select data-agent-setting="${name}" data-agent-layer="${esc(agentLayer.id)}"><option value="">${esc(auto)}</option>${(items||[]).map(item=>`<option value="${esc(item.value)}" ${item.value===current?'selected':''}>${esc(item.name||item.value)}</option>`).join('')}</select></label>`;
    const autoAcceptPick=acp&&artifact.id===agentLayer.id?`<label class="agent-setting" title="Beantwoordt Spin de toestemmingsvragen van deze agent zelf met toestaan? Standaard voor nieuwe Sessions; de knop Auto/Vragen in de chat wijzigt het per Session"><span>Toestemming</span><select data-agent-setting="auto_accept" data-agent-layer="${esc(agentLayer.id)}"><option value="" ${settings.auto_accept==null?'selected':''}>automatisch toestaan</option><option value="true" ${settings.auto_accept===true?'selected':''}>automatisch toestaan</option><option value="false" ${settings.auto_accept===false?'selected':''}>vragen</option></select></label>`:'';
    const settingsLine=acp&&artifact.id===agentLayer.id&&!(options&&!options.error)?`<div class="agent-settings">${autoAcceptPick}</div>`:acp&&options&&!options.error&&artifact.id===agentLayer.id?`<div class="agent-settings">${autoAcceptPick}${pick('mode','Modus',options.modes,settings.mode,'automatisch · full access als de agent dat kent')}<label class="agent-setting"><span>Model</span><input data-agent-setting="model" data-agent-layer="${esc(agentLayer.id)}" list="agent-model-options-${esc(agentLayer.id)}" value="${esc(settings.model||'')}" placeholder="agent default" autocomplete="off" title="Kies uit de lijst of typ een model-id dat de agent kent; leeg is het standaardmodel van je account"><datalist id="agent-model-options-${esc(agentLayer.id)}">${(options.models||[]).map(item=>`<option value="${esc(item.value)}">${esc(item.name||item.value)}${item.description?` · ${esc(item.description)}`:''}</option>`).join('')}</datalist></label>${pick('reasoning_effort','Reasoning',options.reasoning_efforts,settings.reasoning_effort,'agent default')}</div>`:'';
    return `<article class="artifact"><div><div class="artifact-title"><span>${esc(artifactSelector(artifact))}</span><span class="tag">L${artifactLayer(artifact)}</span>${artifactLineage(artifact).version>1?`<span class="tag capability" title="Nieuwe versie van deze laag; oudere versies blijven bestaan voor lagen die erop gebouwd zijn">v${artifactLineage(artifact).version}</span>`:''}<span class="tag ${esc(artifact.kind)}">${esc(artifact.kind)}</span><span class="tag">${esc(identity)}</span>${artifact.sensitivity==='secret'?'<span class="tag secret">secret</span>':''}</div><small title="${esc(artifact.snapshot_digest)}">${esc(artifact.profile)} · ${esc(artifact.snapshot?.driver||'legacy')} · ${esc(String(artifact.snapshot_digest||'').replace(/^sha256:/,'').slice(0,12))}</small><small>parent ${artifactLineage(artifact).parents.map(esc).join(', ')||'Alpine substrate'}</small>${contentsLine(artifact.snapshot?.contents)}${artifact.enables?.length?`<small>ENABLES <span class="capability">${esc(enabledNames(artifact.enables))}</span>${(artifact.enables||[]).filter(item=>item.name==='acp').map(item=>` · commando <input class="command-input" data-command-layer="${esc(artifact.id)}" value="${esc(item.command||'')}" placeholder="bijv. codex-acp of claude-agent-acp" title="Entrypoint van de ACP-agent in deze laag; achteraf aan te passen, EDIT neemt het over">`).join('')}</small>`:''}${optionsLine}${settingsLine}</div><div class="artifact-actions">${use}${fetchOptions}${contentsButton(artifact)}${next}${remove}</div></article>`;
  }).join('');
  bindActionButtons(root);bindArtifactRemove(root);
  root.querySelectorAll('[data-contents-artifact]').forEach(button=>button.onclick=()=>{const artifact=byID(snapshot.artifacts,button.dataset.contentsArtifact);openContents(`/api/artifacts/${encodeURIComponent(button.dataset.contentsArtifact)}/contents`,artifact?artifactSelector(artifact):'laag',artifact);});
  root.querySelectorAll('[data-agent-setting]').forEach(select=>select.onchange=async()=>{const card=select.closest('.artifact'),payload={};card.querySelectorAll('[data-agent-setting]').forEach(item=>{if(item.dataset.agentSetting==='auto_accept'){if(item.value!=='')payload.auto_accept=item.value==='true';}else payload[item.dataset.agentSetting]=item.value;});try{await api(`/api/artifacts/${encodeURIComponent(select.dataset.agentLayer)}/acp/settings`,{method:'PUT',body:JSON.stringify(payload)});await refresh(true);}catch(error){showError(error);}});
  root.querySelectorAll('[data-command-layer]').forEach(input=>{input.onchange=async()=>{const command=input.value.trim();if(!command)return;try{await api(`/api/artifacts/${encodeURIComponent(input.dataset.commandLayer)}/enablements/acp`,{method:'PUT',body:JSON.stringify({command})});await refresh(true);}catch(error){showError(error);}};input.onkeydown=event=>{if(event.key==='Enter'){event.preventDefault();input.blur();}};});
  root.querySelectorAll('[data-fetch-options]').forEach(button=>button.onclick=async()=>{button.disabled=true;button.innerHTML=`${icon('hourglass_top')}Ophalen…`;try{await api(`/api/artifacts/${encodeURIComponent(button.dataset.fetchOptions)}/acp/options`,{method:'POST'});}catch(error){showError(error);await refresh(true);}});
  root.querySelectorAll('[data-record-from]').forEach(button=>button.onclick=()=>openLayerDialog({scope:'user',from:button.dataset.recordFrom}));
  root.querySelectorAll('[data-edit-artifact]').forEach(button=>button.onclick=()=>{const artifact=artifactBySelector(button.dataset.editArtifact);if(!artifact)return;openConsole();editLayer(artifact);});
}

function usableSelectors(){return [...new Set(snapshot.artifacts.filter(canUse).map(artifactSelector))];}
function fillEnvironmentSelect(select,current='',selectors=usableSelectors()){
  select.innerHTML=selectors.length?selectors.map(selector=>`<option value="${esc(selector)}">${esc(selector)}</option>`).join(''):'<option value="">record eerst een Snapshot</option>';
  select.disabled=selectors.length===0;if(selectors.includes(current))select.value=current;
  return selectors.length;
}
function fillWithSelect(select,selected=[],selectors=usableSelectors()){select.innerHTML=selectors.length?selectors.map(selector=>`<option value="${esc(selector)}" ${selected.includes(selector)?'selected':''}>${esc(selector)}</option>`).join(''):'<option disabled>geen extra lagen beschikbaar</option>';select.disabled=!selectors.length;}
function renderEnvironmentOptions(){const select=document.getElementById('environment-selector'),current=select.value,agents=capabilitySelectors('acp');fillEnvironmentSelect(select,current,agents);const templateGit=document.getElementById('template-git'),templateGitCurrent=templateGit.value;fillEnvironmentSelect(templateGit,templateGitCurrent,capabilityProviderSelectors('git'));fillWithSelect(document.getElementById('git-layers'),selectedValues(document.getElementById('git-layers')),projectLayerSelectors());updateJobRecipeCheck();const useSelect=document.getElementById('use-selector'),useCurrent=useSelect.value;const useCount=fillEnvironmentSelect(useSelect,useCurrent);fillWithSelect(document.getElementById('use-with'),selectedValues(document.getElementById('use-with')));document.getElementById('use-submit').disabled=!useCount;}
function myMCP(){return snapshot.mcp_servers.filter(server=>server.operator===currentOperator());}
function fillMCPSelect(select,selected=[]){const servers=myMCP();select.innerHTML=servers.length?servers.map(server=>`<option value="${esc(server.id)}" ${selected.includes(server.id)?'selected':''}>${esc(server.name)} · ${esc(server.transport)}</option>`).join(''):'<option disabled>geen MCP geconfigureerd</option>';select.disabled=!servers.length;}
function renderMCPOptions(){fillMCPSelect(document.getElementById('job-mcp'),selectedValues(document.getElementById('job-mcp')));}
function gitCredentialScope(value,fallback='user'){return ['user','global','public'].includes(value)?value:fallback;}
function myGitAccounts(){return snapshot.git_accounts.filter(account=>gitCredentialScope(account.credential_scope)==='user'&&account.operator===currentOperator());}
function globalGitAccounts(){return snapshot.git_accounts.filter(account=>gitCredentialScope(account.credential_scope)==='global');}
function gitAccountLabel(account){return `${account.provider}:${account.login} · ${account.host}`;}
function gitRemoteHost(remote){try{return new URL(remote).host.toLowerCase();}catch(_){const match=String(remote||'').match(/^git@([^:]+):/);return match?match[1].toLowerCase():'';}}
function gitAccountForRepository(repository){const scope=gitCredentialScope(repository.credential_scope,'public');if(scope==='public')return null;const host=gitRemoteHost(repository.remote_url),provider=repository.provider,accounts=scope==='global'?globalGitAccounts():myGitAccounts();return accounts.filter(account=>String(account.host||'').toLowerCase()===host&&(!['github','gitlab'].includes(provider)||account.provider===provider)).sort((a,b)=>String(b.updated_at||'').localeCompare(String(a.updated_at||'')))[0]||null;}
function resetJobForm(source=null){
  const form=document.getElementById('job-form');form.reset();forkingJobID=source?.id||'';pendingJobSubmission=null;document.getElementById('job-owner').value=currentOperator();document.getElementById('job-forked-from').value=forkingJobID;document.getElementById('job-dialog-title').textContent=source?'Gesloten Job forken':'Nieuwe Job';document.getElementById('job-dialog-description').textContent=source?'Start een vervolg vanaf de remote resultaatbranch, met de eerdere goal, bijlagen en deliverables als context.':'Kies de flow en geef deze uitvoering één duidelijke goal.';const context=document.getElementById('job-fork-context');context.hidden=!source;context.innerHTML=source?`${icon('fork_right')} Vervolg op <strong>${esc(source.title)}</strong> · basis <code>${esc(source.branch)}</code>`:'';renderGitOptions();renderTemplateOptions();renderEnvironmentOptions();fillMCPSelect(document.getElementById('job-mcp'),source?.mcp_server_ids||[]);if(source){form.elements.title.value=`Vervolg: ${source.title}`;form.elements.reference.value=source.reference||'';form.elements.objective.placeholder='Welke bug, feedback of vervolgvraag moet met deze bestaande context worden opgepakt?';form.elements.git_repository_id.value=source.git_repository_id;form.elements.base_ref.value=source.branch;form.elements.base_ref.readOnly=true;if(snapshot.workflow_templates.some(item=>item.id===source.template_id))form.elements.template_id.value=source.template_id;if(capabilitySelectors('acp').includes(source.environment_selector))form.elements.environment_selector.value=source.environment_selector;}else{form.elements.objective.placeholder='Werkende single switch voor darkmode';form.elements.base_ref.readOnly=false;}form.elements.git_repository_id.disabled=Boolean(source);document.getElementById('job-attachments').value='';renderJobAttachmentSelection();setJobStep(1);updateJobRecipeCheck();
}
function openJobFork(id){const source=byID(snapshot.jobs,id);if(!source||!jobIsClosed(source))return;resetJobForm(source);openDialog('job-dialog');requestAnimationFrame(()=>document.getElementById('objective').focus());}
function renderGitOptions(){const job=document.getElementById('job-git'),current=job.value;job.innerHTML=snapshot.git_repositories.length?snapshot.git_repositories.map(repository=>`<option value="${esc(repository.id)}">${esc(repository.name)} · ${esc(repository.default_ref)}</option>`).join(''):'<option value="">configureer eerst Git</option>';job.disabled=!snapshot.git_repositories.length||Boolean(forkingJobID);if(snapshot.git_repositories.some(item=>item.id===current))job.value=current;}
function renderTemplateOptions(){const select=document.getElementById('job-template'),current=select.value;select.innerHTML=snapshot.workflow_templates.length?snapshot.workflow_templates.map(template=>`<option value="${esc(template.id)}">${esc(template.name)} · ${template.phases.length} stappen</option>`).join(''):'<option value="">maak eerst een Template</option>';select.disabled=!snapshot.workflow_templates.length;if(snapshot.workflow_templates.some(item=>item.id===current))select.value=current;}
function updateJobRecipeCheck(){const template=byID(snapshot.workflow_templates,document.getElementById('job-template').value),repository=byID(snapshot.git_repositories,document.getElementById('job-git').value),agentSelector=document.getElementById('environment-selector').value,agent=snapshot.artifacts.find(item=>canUse(item)&&artifactSelector(item)===agentSelector),gitReady=Boolean(template?.git_selector&&capabilityProviderSelectors('git').includes(template.git_selector)),acpReady=Boolean(agent&&artifactEnables(agent,'acp')),layers=repository?.layer_selectors||[],layersReady=layers.every(selector=>snapshot.artifacts.some(item=>canUse(item)&&artifactSelector(item)===selector));document.getElementById('job-recipe-check').innerHTML=`<div class="recipe-part ${gitReady?'ready':'missing'}"><strong>${gitReady?'✓':'×'} Template → GIT</strong><small>${esc(template?.git_selector||'geen Git environment')}</small></div><div class="recipe-part ${layersReady?'ready':'missing'}"><strong>${layersReady?'✓':'×'} Repository → tooling</strong><small>${esc(layers.join(' · ')||'geen extra projectlagen')}</small></div><div class="recipe-part ${acpReady?'ready':'missing'}"><strong>${acpReady?'✓':'×'} Job → ACP</strong><small>${esc(agentSelector||'geen agent')}</small></div>`;document.getElementById('job-submit').disabled=jobSubmitting||!gitReady||!layersReady||!acpReady||!template||!repository;}
function jobWorkedBy(job,operator){return Boolean(job&&operator&&(job.owner===operator||job.assignee===operator));}
function jobTemplate(job){return job.template_snapshot||byID(snapshot.workflow_templates,job.template_id);}
function sessionPreparation(session){return session?(snapshot.preparing||[]).find(item=>item.session_id===session.id):null;}
function sessionPresenceHTML(session){if(!session)return '';
  const preparing=sessionPreparation(session);
  if(preparing){const state=preparationText(preparing,session);return `<small class="session-presence ${state.failed?'offline':'preparing'}" title="${esc(state.text)}">${icon(state.failed?'replay':'progress_activity')}${esc(state.text)}</small>`;}
  if(!session.client_id)return `<small class="session-presence">${esc(preparationText(null,session).text)}</small>`;const client=byID(snapshot.clients,session.client_id);if(client&&['online','draining'].includes(client.status))return `<small class="session-presence">Runner ${esc(client.name)} · ${esc(client.draining?'draining':'online')}</small>`;const name=client?.name||session.client_id,lastSeen=client?.last_seen_at;return `<small class="session-presence offline">${icon('cloud_off')} Geen actieve client · ${esc(name)}${lastSeen?` · ${esc(elapsedSince(lastSeen))} offline`:''}</small>`;}

function assigneeOptions(job){const users=(snapshot.users||[]).filter(user=>!user.archived_at).map(user=>user.username);const names=[...new Set([jobAssignee(job),job.owner,currentOperator(),...users].filter(Boolean))];return names.map(name=>{const user=(snapshot.users||[]).find(item=>item.username===name);return `<option value="${esc(name)}" ${name===jobAssignee(job)?'selected':''}>${esc(user?.display_name||name)}</option>`;}).join('');}
function renderJobs(){
  const root=document.getElementById('jobs'),open=snapshot.jobs.filter(job=>!jobIsClosed(job)),closed=snapshot.jobs.filter(jobIsClosed),mine=open.filter(job=>jobAssignee(job)===currentOperator()),query=jobStateFilter==='closed'?jobSearch.trim().toLowerCase():'',jobs=(jobStateFilter==='closed'?closed:jobStateFilter==='all'?open:mine).filter(job=>jobMatchesSearch(job,query));
  document.getElementById('job-count-mine').textContent=mine.length;document.getElementById('job-count-all').textContent=open.length;document.getElementById('job-count-closed').textContent=closed.length;
  if(!jobs.length){root.innerHTML=`<div class="empty">${query?`Geen gesloten Job met “${esc(jobSearch.trim())}” in naam, referentie of branch.`:jobStateFilter==='closed'?'Nog geen afgeronde of gesloten Jobs.':jobStateFilter==='mine'?'Niets ligt bij jou. Kijk onder Alle, of start een nieuwe Job.':'Geen open Jobs. Start een nieuwe Job zodra er werk klaarstaat.'}</div>`;return;}
  const cards=jobs.map(job=>{
    const sessions=snapshot.sessions.filter(session=>session.job_id===job.id),repository=byID(snapshot.git_repositories,job.git_repository_id),template=jobTemplate(job),runs=snapshot.phase_runs.filter(run=>run.job_id===job.id),hasComparisonWorkspace=sessions.some(session=>Boolean(byID(snapshot.compositions,session.prepared_composition_id)?.runtime));
    const workflowStatus=job.status==='cancelled'?'GESLOTEN':job.workflow_status==='pending'?`PENDING · ${job.pending_reason==='ask'?'ASK':'USER'}`:job.workflow_status==='busy'?'BEZIG':job.workflow_status==='done'?'KLAAR':String(job.status||'').toUpperCase();
    const statusClass=job.workflow_status==='pending'?'status-pending':job.workflow_status==='busy'?'status-busy':'';
    const deliverableChips=latestDeliverables(job.id).map(item=>`<button class="deliverable-chip" type="button" data-deliverable="${esc(item.id)}">${esc(item.name)} · r${item.revision}</button>`).join(''),attachmentChips=jobAttachments(job.id).map(attachmentHTML).join('');
    const runHTML=runs.map(run=>{
      const session=byID(sessions,run.session_id),composition=byID(snapshot.compositions,session?.prepared_composition_id),ready=composition?.runtime&&composition.runtime.status==='ready',acp=(composition?.enabled||[]).some(item=>item.name==='acp'),active=job.current_phase_run_id===run.id&&['queued','running','pending'].includes(run.status),stepIsRunning=run.status==='running',stepDetailKey=`phase:${run.id}`,question=snapshot.workflow_questions.find(item=>item.phase_run_id===run.id&&item.status==='open'),approvals=snapshot.workflow_questions.filter(item=>item.phase_run_id===run.id&&item.kind==='approval'),outputs=snapshot.deliverables.filter(item=>item.phase_run_id===run.id),retryable=session&&session.executor!=='action'&&jobWorkedBy(job,currentOperator())&&active,decisionHTML=workflowDecisionHistory(run,approvals,job),actionResult=run.action_result,phase=(template?.phases||[]).find(item=>item.id===run.phase_id),allowsChanges=Boolean(phase&&(phase.allow_changes||phase.allow_commit)),canViewStepChanges=allowsChanges&&session&&((active&&ready)||(run.status==='accepted'&&hasComparisonWorkspace)),presenceHTML=sessionPresenceHTML(session);
      const changeAction=allowsChanges?`<button class="small-button" data-job-changes="${esc(job.id)}" data-change-session="${esc(session?.id||'')}" data-change-live="${active&&ready?'true':'false'}" ${canViewStepChanges?'':'disabled'} title="${canViewStepChanges?'Bekijk alleen de changes van deze stap':'Geen actieve Git-workspace beschikbaar'}">${icon('difference')}Changes</button>`:'';
      const appServices=(repository?.services||[]),appPanel=appServices.length&&active&&ready&&session?`<details class="app-panel" data-app-session="${esc(session.id)}" data-detail-state="app:${esc(session.id)}" ${detailOpenAttribute(`app:${session.id}`,session.executor==='expose')}><summary>${icon('rocket_launch')}Test-app · ${appServices.map(service=>esc(service.name)).join(', ')}</summary><div class="app-panel-body" data-app-body>Status ophalen…</div></details>`:'';
      const actions=`${changeAction}${active&&ready&&acp?`<button class="small-button" data-open-acp="${esc(session.id)}">${icon('chat')}Chat</button>`:''}${retryable?`<button class="small-button" data-retry-session="${esc(session.id)}">${icon('replay')}Retry</button>`:''}`;
      const agentWorking=Boolean(question)&&run.status==='running',lock=agentWorking?' disabled title="De agent werkt nog in de chat · de knoppen komen terug zodra de beurt eindigt"':'';
      const answerButton=question?.kind==='agent'&&question.items?.length?`<button class="primary" data-answer-question="${esc(question.id)}"${lock}>${icon('send')}BEANTWOORD</button>`:'';
      const questionButtons=question?.kind==='action'?`<button class="primary" data-accept-question="${esc(question.id)}"${lock}>${icon('replay')}OPNIEUW</button>`:`${answerButton}${acceptButtons(question,answerButton?'small-button':'primary',lock)}<button class="${answerButton?'small-button':'danger'}" data-reject-question="${esc(question?.id)}"${lock}>${icon('cancel')}REJECT</button>${active&&ready&&acp?`<button class="small-button" data-open-acp="${esc(session.id)}">${icon('chat')}CHAT</button>`:''}`;
      const questionHTML=question?`<div class="question-card" data-workflow-question="${esc(question.id)}"><strong>${workflowDecisionTitle(question)}</strong>${question.kind==='approval'?decisionHTML:agentQuestionFormHTML(question)}${workflowDecisionRoutes(question,template)}${agentWorking?`<small class="question-hint">${icon('progress_activity')}De agent is bezig in de chat · beslissen kan zodra de beurt eindigt</small>`:''}<div class="panel-actions">${questionButtons}</div></div>`:'';
      const actionHTML=actionResult?(actionResult.type==='git.merge'?`<div class="workflow-result"><div class="workflow-result-copy">${icon('merge')}<div><strong>Gemerged in ${esc(job.base_ref||'basisbranch')}</strong><small>${esc(actionResult.detail||'')}</small></div></div>${actionResult.url?`<a class="workflow-result-link" href="${esc(actionResult.url)}" target="_blank" rel="noopener noreferrer">Commit ${icon('open_in_new')}</a>`:''}</div>`:`<div class="workflow-result"><div class="workflow-result-copy">${icon('merge')}<div><strong>Pull request #${esc(actionResult.external_id||'')}</strong><small>Remote resultaat aangemaakt</small></div></div><a class="workflow-result-link" href="${esc(actionResult.url)}" target="_blank" rel="noopener noreferrer">Openen ${icon('open_in_new')}</a></div>`):'';
      const actionFailure=session?.executor==='action'&&run.reject_reason&&!question?`<p class="warning">${esc(run.reject_reason)}</p>`:'';
      const head=`<span class="workflow-marker ${esc(run.status)}"></span><span class="workflow-step-copy"><strong>${esc(run.phase_name)} <span class="tag">poging ${run.attempt}</span>${session?.executor==='action'?'<span class="tag capability">ACTION</span>':''}</strong><small>${session?short(session.id):'geen Session'}</small>${active?presenceHTML:''}</span><span class="workflow-step-status">${esc(sessionPreparation(session)?'voorbereiden':run.status)}</span>`;
      const body=`<div class="workflow-step-body"><div class="workflow-step-content">${outputs.length?`<div class="deliverable-chips">${outputs.map(item=>`<button class="deliverable-chip" data-deliverable="${esc(item.id)}">${esc(item.name)} · r${item.revision}</button>`).join('')}</div>`:''}${actionHTML}${actionFailure}${appPanel}${questionHTML}${question?.kind==='approval'?'':decisionHTML}</div>${actions?`<div class="session-actions">${actions}</div>`:''}</div>`;
      // A step that waits on a decision is not something to fold away: it
      // is shown whole, always, under the same head as every other step.
      if(question)return `<section class="workflow-step decision ${stepIsRunning?'active':''}"><div class="workflow-step-head">${head}</div>${body}</section>`;
      return `<details class="workflow-step ${stepIsRunning?'active':''}" data-detail-state="${esc(stepDetailKey)}" ${detailOpenAttribute(stepDetailKey,stepIsRunning)}><summary>${head}</summary>${body}</details>`;
    }).join('');
    const legacySessions=!template?sessions.map(session=>{const composition=byID(snapshot.compositions,session.prepared_composition_id),ready=composition?.runtime&&composition.runtime.status!=='stopped',acp=(composition?.enabled||[]).some(item=>item.name==='acp');return `<div class="session"><div><strong>${esc(session.role||'worker')}</strong><small>${esc(session.git_ref)}</small>${sessionPresenceHTML(session)}</div><div class="session-actions"><span class="tag">${ready?'capsule ready':esc(session.status)}</span>${ready&&acp?`<button class="primary" data-open-acp="${esc(session.id)}">Open chat</button>`:`<button class="small-button" data-action="use-session" data-id="${esc(session.id)}">Run</button>`}</div></div>`;}).join(''):'';
    const detailKey=`job:${job.id}`,createdAt=formatDateTime(job.created_at);
    const referenced=snapshot.jobs.some(candidate=>candidate.forked_from_job_id===job.id),isOwner=job.owner===currentOperator(),latestTemplate=byID(snapshot.workflow_templates,job.template_id),newerTemplate=!jobIsClosed(job)&&latestTemplate&&template&&(latestTemplate.revision||1)>(template.revision||1)?latestTemplate:null,changeAction=`<button class="small-button" type="button" data-job-changes="${esc(job.id)}" ${hasComparisonWorkspace?'':'disabled'} title="${hasComparisonWorkspace?'Bekijk alle Job changes sinds de basisbranch':'De eerste Git-workspace is nog niet gereed'}">${icon('difference')}Changes</button>`,ownerActions=jobIsClosed(job)?`<button class="small-button" type="button" data-fork-job="${esc(job.id)}">${icon('fork_right')}Fork</button>${isOwner?`<button class="icon-button danger" type="button" data-remove-job="${esc(job.id)}" ${referenced?'disabled':''} title="${referenced?'Deze Job levert context aan een vervolg-Job':'Job definitief verwijderen'}" aria-label="Job definitief verwijderen">${icon('delete')}</button>`:''}`:(isOwner?`<button class="small-button" type="button" data-job-environment="${esc(job.id)}" title="Agent/environment en MCP-connecties voor de volgende stappen">${icon('tune')}Omgeving</button>${newerTemplate?`<button class="small-button" type="button" data-adopt-template="${esc(job.id)}" title="Deze Job draait op revisie ${template?.revision||1}; het Template staat op revisie ${newerTemplate.revision}">${icon('upgrade')}Template r${newerTemplate.revision}</button>`:''}<button class="small-button" type="button" data-add-job-attachment="${esc(job.id)}" title="PDF of afbeelding toevoegen">${icon('attach_file')}Bijlage</button><button class="small-button" type="button" data-close-job="${esc(job.id)}">${icon('archive')}Sluiten</button>`:'');
    const source=byID(snapshot.jobs,job.forked_from_job_id);return {key:job.id,html:`<article class="job ${jobIsClosed(job)?'closed':''}"><div class="job-head"><div><div class="job-title-row"><h3>${esc(job.title)}</h3>${createdAt?`<span class="job-created" title="Aangemaakt ${esc(createdAt)}">${icon('calendar_today')}${esc(createdAt)}</span>`:''}</div><div class="job-objective md">${job.objective?markdown(job.objective):'<p class="hint">Goal wordt in de brainstorm bepaald.</p>'}</div><div class="meta"><span class="tag ${statusClass}">${esc(workflowStatus)}</span>${jobIsClosed(job)?`<span class="tag">${icon('person')} ${esc(jobAssignee(job))}</span>`:`<label class="tag assignee-tag" title="Bij wie ligt deze Job">${icon('person')}<select data-assign-job="${esc(job.id)}">${assigneeOptions(job)}</select></label>`}${job.reference?`<span class="tag reference" title="Referentie">${icon('tag')} ${esc(job.reference)}</span>`:''}${source?`<span class="tag capability">${icon('fork_right')} ${esc(source.title)}</span>`:''}${template?`<span class="tag">${esc(template.name)} · r${template.revision||1}</span>`:''}<span class="tag branch">${esc(job.branch)}</span><span class="tag">${esc(repository?.name||'Git ontbreekt')}@${esc(job.base_ref)}</span></div>${attachmentChips?`<div class="attachment-selection">${attachmentChips}</div>`:''}${deliverableChips?`<div class="deliverable-chips">${deliverableChips}</div>`:''}</div><div class="job-head-actions">${changeAction}${ownerActions}</div></div><details class="job-detail" data-job-detail="${esc(job.id)}" data-detail-state="${esc(detailKey)}" ${detailOpenAttribute(detailKey,job.workflow_status==='busy')}><summary>${runs.length||sessions.length} stap${(runs.length||sessions.length)===1?'':'pen'} · workflow en Sessions</summary><div class="job-detail-body">${template?runHTML:`<div class="sessions">${legacySessions}</div>`}</div></details></article>`};
  });
  const bind=()=>{bindDetailStates(root);bindActionButtons(root);bindACPButtons(root);bindDeliverables(root);bindQuestionButtons(root);bindResultButtons(root);bindAppPanels(root);
  root.querySelectorAll('[data-assign-job]').forEach(select=>select.onchange=async()=>{try{await api(`/api/jobs/${encodeURIComponent(select.dataset.assignJob)}/assignee`,{method:'PUT',body:JSON.stringify({assignee:select.value})});await refresh(true);}catch(error){showError(error);await refresh(true);}});};
  root._afterPatch=bind;if(patchKeyed(root,cards))bind();root.querySelectorAll('[data-job-changes]').forEach(button=>button.onclick=()=>openJobChanges(button));root.querySelectorAll('[data-adopt-template]').forEach(button=>button.onclick=()=>openAdoptTemplate(button.dataset.adoptTemplate));root.querySelectorAll('[data-job-environment]').forEach(button=>button.onclick=()=>openJobEnvironment(button.dataset.jobEnvironment));root.querySelectorAll('[data-add-job-attachment]').forEach(button=>button.onclick=()=>chooseJobAttachments(button.dataset.addJobAttachment));root.querySelectorAll('[data-close-job]').forEach(button=>button.onclick=()=>closeJob(button));root.querySelectorAll('[data-fork-job]').forEach(button=>button.onclick=()=>openJobFork(button.dataset.forkJob));root.querySelectorAll('[data-remove-job]').forEach(button=>button.onclick=()=>removeJob(button.dataset.removeJob));root.querySelectorAll('[data-retry-session]').forEach(button=>button.onclick=()=>retrySession(button));
}

// The test-app panel reads live status from the runner while it is open
// (it is a server/client model; nothing here is realtime) and keeps what it
// last saw so a re-render does not flash.
const appPanelState=new Map();
function appServiceRowHTML(service,live){
  const status=live?(live.status==='running'?(live.reachable?'bereikbaar':'gestart, wacht op de poort'):live.status):'niet gestart',cls=live?(live.status==='running'?(live.reachable?'up':'starting'):'down'):'down';
  const links=live?.ports?Object.entries(live.ports).map(([containerPort,hostPort])=>`<a href="http://${esc(live.host)}:${hostPort}" target="_blank" rel="noopener noreferrer">http://${esc(live.host)}:${hostPort}</a> <small>(:${esc(containerPort)})</small>`).join(' · '):'';
  return `<div class="app-service"><span class="app-dot ${cls}"></span><strong>${esc(service.name)}</strong><span class="app-status">${esc(status)}${live?.error&&live.status!=='running'?` · ${esc(live.error)}`:''}</span><span class="app-links">${links}</span>${live?`<button class="small-button" type="button" data-app-logs="${esc(service.name)}">${icon('subject')}Logs</button>`:''}</div>`;
}
async function refreshAppPanel(panel){
  const sessionID=panel.dataset.appSession,body=panel.querySelector('[data-app-body]');if(!body)return;
  try{
    const status=await api(`/api/sessions/${encodeURIComponent(sessionID)}/app`);appPanelState.set(sessionID,status);
    const anyRunning=(status.running||[]).some(item=>item.status==='running');
    const head=status.start==='running'?`<div class="app-note">${icon('progress_activity')}Starten… (voorbereidende commando's draaien in de container; volg de logs)</div>`:status.error?`<div class="app-note error">${esc(status.error)}</div>`:'';
    body.innerHTML=`${head}${status.services.map(service=>appServiceRowHTML(service,(status.running||[]).find(item=>item.service===service.name))).join('')}<div class="panel-actions" style="margin-top:8px"><button class="primary" type="button" data-app-start ${status.start==='running'?'disabled':''}>${icon('play_arrow')}${anyRunning?'Herstart':'Start'}</button>${anyRunning?`<button class="danger" type="button" data-app-stop>${icon('stop')}Stop</button>`:''}</div>`;
    body.querySelector('[data-app-start]').onclick=async()=>{body.querySelector('[data-app-start]').disabled=true;try{await api(`/api/sessions/${encodeURIComponent(sessionID)}/app/start`,{method:'POST'});}catch(error){showError(error);}await refreshAppPanel(panel);};
    const stop=body.querySelector('[data-app-stop]');if(stop)stop.onclick=async()=>{stop.disabled=true;try{await api(`/api/sessions/${encodeURIComponent(sessionID)}/app/stop`,{method:'POST'});}catch(error){showError(error);}await refreshAppPanel(panel);};
    body.querySelectorAll('[data-app-logs]').forEach(button=>button.onclick=()=>openAppLogs(sessionID,button.dataset.appLogs));
  }catch(error){body.innerHTML=`<div class="app-note error">${esc(error.message||error)}</div>`;}
}
let appPanelTimer=null;
function bindAppPanels(root){
  const panels=[...root.querySelectorAll('details[data-app-session]')];
  panels.forEach(panel=>{const previous=appPanelState.get(panel.dataset.appSession);if(previous&&panel.open){const body=panel.querySelector('[data-app-body]');body.innerHTML=body.innerHTML;}panel.addEventListener('toggle',()=>{if(panel.open)refreshAppPanel(panel);});if(panel.open)refreshAppPanel(panel);});
  clearInterval(appPanelTimer);appPanelTimer=setInterval(()=>document.querySelectorAll('details[data-app-session][open]').forEach(refreshAppPanel),4000);
}
let appLogsTimer=null;
function openAppLogs(sessionID,service){
  const output=document.getElementById('app-logs-output');document.getElementById('app-logs-title').textContent=`Logs · ${service}`;output.textContent='Laden…';openDialog('app-logs-dialog');
  const load=async()=>{if(!document.getElementById('app-logs-dialog').open){clearInterval(appLogsTimer);return;}try{const logs=await api(`/api/sessions/${encodeURIComponent(sessionID)}/app/${encodeURIComponent(service)}/logs?tail=300`);const atBottom=output.scrollTop+output.clientHeight>=output.scrollHeight-20;output.textContent=logs.output||'(nog geen output)';if(atBottom)output.scrollTop=output.scrollHeight;}catch(error){output.textContent=error.message||String(error);}};
  clearInterval(appLogsTimer);load();appLogsTimer=setInterval(load,3000);
}
function renderTemplates(){const root=region('templates');if(!snapshot.workflow_templates.length){root.innerHTML='<div class="empty">Nog geen Templates. Maak bijvoorbeeld Ontwikkeling met Ontwerp → Ontwikkelen → Review.</div>';return;}root.innerHTML=snapshot.workflow_templates.map(template=>`<article class="template-card"><div class="job-head"><div><h3>${esc(template.name)} <span class="tag">r${template.revision||1}</span>${template.git_selector?` <span class="tag capability">GIT · ${esc(template.git_selector)}</span>`:''}</h3><p>${esc(template.description||'Eigen workflow')}</p></div><div class="panel-actions">${template.created_by===currentOperator()?`<button class="small-button" data-edit-template="${esc(template.id)}">${icon('edit')}Nieuwe revisie</button>`:''}<button class="small-button" data-copy-template="${esc(template.id)}" title="Nieuw Template met dezelfde stappen">${icon('content_copy')}Kopie</button>${template.created_by===currentOperator()?`<button class="danger" data-remove-template="${esc(template.id)}">${icon('delete')}Verwijder</button>`:''}</div></div><div class="template-flow">${template.phases.map((phase,index)=>`${index?'<span class="phase-arrow">→</span>':''}<span class="phase-pill">${index+1}. ${esc(phase.name)}${phase.executor==='action'?' · ACTION':phase.environment_selector?' · '+esc(phase.environment_selector):' · JOB DEFAULT'}${phase.allow_changes||phase.allow_commit?' · WRITE':''}${(phase.inject||[]).length?' · IN '+esc(phase.inject.join('+')):''}${phase.accept?.ask_user?' · A:USER':''}${phase.reject?.ask_user?' · R:USER':''}</span>`).join('')}</div></article>`).join('');root.querySelectorAll('[data-edit-template]').forEach(button=>button.onclick=()=>openTemplateEditor(button.dataset.editTemplate));root.querySelectorAll('[data-copy-template]').forEach(button=>button.onclick=()=>openTemplateCopy(button.dataset.copyTemplate));root.querySelectorAll('[data-remove-template]').forEach(button=>button.onclick=()=>removeTemplate(button.dataset.removeTemplate));}

// DONE ends the Job; a merge or a pull request is a step like any other.
function templateTargetOptions(selected='',includeSelf=true){const steps=[...document.querySelectorAll('[data-template-step]')],options=[['NEXT','Volgende stap']];if(includeSelf)options.push(['SELF','Dezelfde stap']);steps.forEach((step,index)=>options.push([step.dataset.stepId,`${index+1}. ${step.querySelector('[name=phase_name]').value||'Naamloze stap'}`]));options.push(['DONE','Klaar · Job afronden']);return options.map(([value,label])=>`<option value="${esc(value)}" ${value===selected?'selected':''}>${esc(label)}</option>`).join('');}
function refreshTemplateTargets(){document.querySelectorAll('[data-transition-target]').forEach(select=>{const current=select.value,includeSelf=select.dataset.transitionTarget==='reject';select.innerHTML=templateTargetOptions(current,includeSelf);if([...select.options].some(option=>option.value===current))select.value=current;});}
// The step kinds: what an agent does, what a person tests, and what Spin
// performs itself.
const stepKinds=[['agent','Agent · ACP-sessie'],['expose','Test-app · start de services van de repository en wacht op jouw oordeel'],['merge','Mergen · Spin merget de Job-branch in de basisbranch; een conflict is een REJECT'],['pull_request','Pull request · Spin maakt een pull request op de remote']];
function stepKindOf(values){if(values.executor==='action')return values.action?.type==='git.merge'?'merge':'pull_request';return values.executor||'agent';}
function stepKindFields(kind){return kind==='merge'||kind==='pull_request'?{executor:'action',action:{type:kind==='merge'?'git.merge':'git.pull_request.create'}}:{executor:kind};}
function refreshTemplateInjections(){const steps=[...document.querySelectorAll('[data-template-step]')];steps.forEach((step,index)=>{const root=step.querySelector('[data-inject-deliverables]'),available=new Map();steps.slice(0,index).forEach(previous=>previous.querySelectorAll('[name=deliverable_name]').forEach(input=>{const name=input.value.trim(),key=name.toLowerCase();if(name&&!available.has(key))available.set(key,name);}));if(!step._injectSelection)step._injectSelection=new Set();root.innerHTML=available.size?[...available].map(([key,name])=>`<label class="inject-option"><input type="checkbox" name="inject_deliverable" value="${esc(name)}" ${step._injectSelection.has(key)?'checked':''}> ${esc(name)}</label>`).join(''):'<span class="hint">Nog geen deliverables uit eerdere stappen.</span>';root.querySelectorAll('[name=inject_deliverable]').forEach(input=>input.onchange=()=>{const key=input.value.trim().toLowerCase();if(input.checked)step._injectSelection.add(key);else step._injectSelection.delete(key);});});}
// What a deliverable must be: checked when the agent puts it.
const deliverableKinds=[['markdown','Document (Markdown)'],['pdf','PDF'],['image','Afbeelding'],['folder','Map (site met eigen CSS/JS, of losse bestanden)'],['file','Bestand']];
function addDeliverableRow(container,values={}){const row=document.createElement('div');row.className='deliverable-row';row.innerHTML=`<input name="deliverable_name" required placeholder="FO" value="${esc(values.name||'')}"><input name="deliverable_description" placeholder="Functioneel ontwerp" value="${esc(values.description||'')}"><select name="deliverable_kind" title="Wat de agent moet opleveren; Spin controleert het bij het opleveren">${deliverableKinds.map(([value,label])=>`<option value="${value}" ${(values.kind||'markdown')===value?'selected':''}>${esc(label)}</option>`).join('')}</select><label style="margin:0"><input name="deliverable_required" type="checkbox" style="width:auto" ${values.required===false?'':'checked'}> verplicht</label><button class="icon-button" type="button" title="Verwijder">${icon('delete')}</button>`;row.querySelector('button').onclick=()=>{row.remove();refreshTemplateInjections();};row.querySelector('[name=deliverable_name]').oninput=refreshTemplateInjections;container.appendChild(row);refreshTemplateInjections();}
function addTemplateStep(values={}){
  if(values.id){const sequence=String(values.id).match(/^step-(\d+)$/);if(sequence)templateStepSequence=Math.max(templateStepSequence,Number(sequence[1]));}const id=values.id||`step-${++templateStepSequence}`,root=document.getElementById('template-steps'),step=document.createElement('section');
  step.className='template-step-editor';step.dataset.templateStep='';step.dataset.stepId=id;
  step._injectSelection=new Set((values.inject||[]).map(name=>String(name).trim().toLowerCase()));
  step.innerHTML=`<div class="template-step-head"><strong>Stap <span data-step-number></span></strong><button class="danger" type="button" data-remove-step>${icon('delete')}Verwijder stap</button></div><div class="template-step-body"><div><label>Naam</label><input name="phase_name" required placeholder="Ontwerp" value="${esc(values.name||'')}"></div><div><label>Soort</label><select name="phase_executor">${stepKinds.map(([value,label])=>`<option value="${value}" ${stepKindOf(values)===value?'selected':''}>${esc(label)}</option>`).join('')}</select></div><div><label>Environment</label><select name="phase_environment"></select><small>Leeg gebruikt de agent van de Job (daar kies je het model). Een laag zonder agent, zoals tool:git, komt als extra laag bij die agent.</small></div><div class="wide template-runtime"><div><label>Extra lagen</label><select name="phase_with" multiple size="4"></select><small>Alleen deze stap krijgt deze extra lagen.</small></div><div><label>Beleid</label><label style="padding:6px 0;margin:0"><input name="allow_changes" type="checkbox" style="width:auto" ${values.allow_changes||values.allow_commit?'checked':''}> wijzigingen gaan mee bij ACCEPT</label><small>Aan: Spin vouwt de wijzigingen bij definitieve ACCEPT tot één commit op de Job-branch. Uit: de agent mag in zijn workspace bouwen en experimenteren, maar niets gaat mee en ACCEPT bevestigt alleen de basis.</small></div><div class="wide template-runtime"><div><label>Model</label><input name="phase_model" list="agent-model-options" placeholder="agent default" value="${esc(values.model||'')}" autocomplete="off"><small>Uit de opties van de ACP-laag (Environments → Opties ophalen). Leeg laat de agent kiezen.</small></div><div><label>Reasoning</label><input name="phase_reasoning" list="agent-effort-options" placeholder="agent default" value="${esc(values.reasoning_effort||'')}" autocomplete="off"><small>Denkniveau voor deze stap, bijvoorbeeld low voor routinewerk.</small></div></div><div class="wide"><label>Omschrijving / opdracht voor de agent</label><textarea name="instructions" required placeholder="Je maakt het functioneel en technisch ontwerp.">${esc(values.instructions||'')}</textarea></div><div class="wide"><label>Context injecteren</label><div class="inject-editor" data-inject-deliverables></div><small>De Job Goal gaat altijd mee. Aangevinkte documenten worden verplicht als laatste revisie opgenomen.</small></div><div class="wide"><label>Deliverables opleveren</label><div class="deliverable-editor" data-deliverables></div><button class="small-button" type="button" data-add-deliverable style="margin-top:7px">${icon('add')}Document</button></div></div><div class="wide"><label>Overgangen</label><div class="transition-editor"><div class="transition-route"><label>ACCEPT →</label><select name="accept_target" data-transition-target="accept"></select><label class="transition-gate"><input name="accept_ask_user" type="checkbox" ${(values.accept?.ask_user??values.ask_user)?'checked':''}> Ask user</label></div><div class="transition-route"><label>REJECT →</label><select name="reject_target" data-transition-target="reject"></select><label class="transition-gate"><input name="reject_ask_user" type="checkbox" ${(values.reject?.ask_user??values.ask_user)?'checked':''}> Ask user</label></div><div><label>REJECT max</label><input name="reject_max" type="number" min="0" value="${values.reject?.max??2}"></div><div><label>Na maximum</label><div class="hint" style="padding:10px 0">ASK USER</div></div></div></div></div>`;
  root.appendChild(step);(values.deliverables||[]).forEach(item=>addDeliverableRow(step.querySelector('[data-deliverables]'),item));
  const executor=step.querySelector('[name=phase_executor]'),agentOnly=()=>{const plain=executor.value!=='agent';step.querySelectorAll('[data-agent-only]').forEach(node=>node.hidden=plain);step.querySelector('[name=instructions]').required=!plain;refreshTemplateTargets();};executor.onchange=agentOnly;
  step.querySelectorAll('.template-step-body > div').forEach(node=>{if(node.querySelector('[name=phase_with],[name=phase_model],[name=instructions],[name=inject_deliverable],[data-deliverables],[data-inject-deliverables]'))node.dataset.agentOnly='';});agentOnly();
  step.querySelector('[data-add-deliverable]').onclick=()=>addDeliverableRow(step.querySelector('[data-deliverables]'));
  step.querySelector('[data-remove-step]').onclick=()=>{step.remove();renumberTemplateSteps();};step.querySelector('[name=phase_name]').oninput=refreshTemplateTargets;
  // What a step already has stays selectable, whatever the lists below
  // happen to offer: an editor must never drop a value it did not show.
  const environment=step.querySelector('[name=phase_environment]'),environmentValue=values.environment_selector||'',environmentSelectors=[...new Set([...capabilitySelectors('acp'),...capabilityProviderSelectors('git'),...(environmentValue?[environmentValue]:[])])];environment.innerHTML='<option value="">Job default</option>'+environmentSelectors.map(selector=>`<option value="${esc(selector)}">${esc(selector)}</option>`).join('');environment.value=environmentValue;
  fillWithSelect(step.querySelector('[name=phase_with]'),values.with_selectors||[],[...new Set([...usableSelectors(),...(values.with_selectors||[])])]);
  renumberTemplateSteps();refreshTemplateTargets();const steps=[...document.querySelectorAll('[data-template-step]')],index=steps.indexOf(step),previous=index>0?steps[index-1].dataset.stepId:'SELF';
  const isDone=target=>target==='DONE'||String(target||'').startsWith('DONE:'),own=values.executor==='action';
  step.querySelector('[name=accept_target]').value=isDone(values.accept?.target)?'DONE':values.accept?.target||(own?'DONE':'NEXT');step.querySelector('[name=reject_target]').value=isDone(values.reject?.target)?'DONE':values.reject?.target||previous;
  refreshTemplateInjections();
}
function renumberTemplateSteps(){document.querySelectorAll('[data-template-step]').forEach((step,index)=>step.querySelector('[data-step-number]').textContent=index+1);refreshTemplateTargets();refreshTemplateInjections();}
function agentOptionDatalists(){
  const models=new Map(),efforts=new Map();
  snapshot.artifacts.forEach(artifact=>{const options=artifact.agent_options;if(!options)return;(options.models||[]).forEach(item=>models.set(item.value,`${item.name||item.value} · ${artifactSelector(artifact)}`));(options.reasoning_efforts||[]).forEach(item=>efforts.set(item.value,item.name||item.value));});
  const fill=(id,entries)=>{let list=document.getElementById(id);if(!list){list=document.createElement('datalist');list.id=id;document.body.appendChild(list);}list.innerHTML=[...entries].map(([value,label])=>`<option value="${esc(value)}">${esc(label)}</option>`).join('');};
  fill('agent-model-options',models);fill('agent-effort-options',efforts);
}
function resetTemplateForm(template=null){const form=document.getElementById('template-form');form.reset();agentOptionDatalists();document.getElementById('template-steps').innerHTML='';templateStepSequence=0;editingTemplateID=template?.id||'';document.getElementById('template-dialog-title').textContent=template?'Workflow Template wijzigen':'Nieuw Workflow Template';document.getElementById('template-submit').textContent=template?'Wijzigingen bewaren':'Template bewaren';if(template){form.elements.name.value=template.name||'';form.elements.description.value=template.description||'';form.elements.git_selector.value=template.git_selector||form.elements.git_selector.value;(template.phases||[]).forEach(phase=>addTemplateStep(phase));}else addTemplateStep();}
function openTemplateEditor(id){const template=byID(snapshot.workflow_templates,id);if(!template)return;resetTemplateForm(template);openDialog('template-dialog');}
// A copy is a new Template with the same steps, named after the original.
function openTemplateCopy(id){const template=byID(snapshot.workflow_templates,id);if(!template)return;resetTemplateForm({...template,id:'',revision:0,name:`${template.name} (kopie)`});document.getElementById('template-dialog-title').textContent='Template kopiëren';document.getElementById('template-submit').textContent='Kopie bewaren';openDialog('template-dialog');requestAnimationFrame(()=>document.getElementById('template-form').elements.name.select());}
function collectTemplatePhases(){return [...document.querySelectorAll('[data-template-step]')].map(step=>({id:step.dataset.stepId,name:step.querySelector('[name=phase_name]').value,...stepKindFields(step.querySelector('[name=phase_executor]').value),instructions:step.querySelector('[name=instructions]').value,model:step.querySelector('[name=phase_model]').value.trim(),reasoning_effort:step.querySelector('[name=phase_reasoning]').value.trim(),environment_selector:step.querySelector('[name=phase_environment]').value,with_selectors:selectedValues(step.querySelector('[name=phase_with]')),allow_changes:step.querySelector('[name=allow_changes]').checked,inject:[...step.querySelectorAll('[name=inject_deliverable]:checked')].map(input=>input.value),deliverables:[...step.querySelectorAll('.deliverable-row')].map(row=>({name:row.querySelector('[name=deliverable_name]').value,description:row.querySelector('[name=deliverable_description]').value,kind:row.querySelector('[name=deliverable_kind]').value,required:row.querySelector('[name=deliverable_required]').checked})),accept:{target:step.querySelector('[name=accept_target]').value,ask_user:step.querySelector('[name=accept_ask_user]').checked},reject:{target:step.querySelector('[name=reject_target]').value,ask_user:step.querySelector('[name=reject_ask_user]').checked,max:Number(step.querySelector('[name=reject_max]').value||0),exhausted:'ASK_USER'}}));}

function addServiceRow(container,values={}){
  const row=document.createElement('div');row.className='service-row';
  row.innerHTML=`<div class="service-row-head"><input name="service_name" required placeholder="web" value="${esc(values.name||'')}" title="Servicenaam (letters, cijfers, streepjes)"><input name="service_ports" placeholder="poorten, bv. 3000 5173" value="${esc((values.ports||[]).join(' '))}"><input name="service_env" placeholder="env-naam (var/env/<naam>.env)" value="${esc(values.env||'')}"><button class="icon-button" type="button" title="Verwijder">${icon('delete')}</button></div><div class="service-row-body"><div><label>Voorbereiden (één commando per regel)</label><textarea name="service_prepare" rows="2" placeholder="npm ci">${esc((values.prepare||[]).join('\n'))}</textarea></div><div><label>Run</label><input name="service_run" placeholder="npm run start" value="${esc(values.run||'')}"><label style="margin-top:6px">…of image (dependency)</label><input name="service_image" placeholder="postgres:16" value="${esc(values.image||'')}"></div></div>`;
  row.querySelector('button').onclick=()=>row.remove();container.appendChild(row);
}
function collectServices(){return [...document.querySelectorAll('#git-services .service-row')].map(row=>({name:row.querySelector('[name=service_name]').value.trim(),ports:row.querySelector('[name=service_ports]').value.split(/[\s,]+/).map(Number).filter(port=>port>0),env:row.querySelector('[name=service_env]').value.trim(),prepare:row.querySelector('[name=service_prepare]').value.split('\n').map(line=>line.trim()).filter(Boolean),run:row.querySelector('[name=service_run]').value.trim(),image:row.querySelector('[name=service_image]').value.trim()}));}
function resetGitForm(repository=null){
  const form=document.getElementById('git-form'),layers=document.getElementById('git-layers');form.reset();editingGitRepositoryID=repository?.id||'';
  const services=document.getElementById('git-services');services.innerHTML='';(repository?.services||[]).forEach(service=>addServiceRow(services,service));document.getElementById('git-add-service').onclick=()=>addServiceRow(services);
  form.elements.service_hosts.value=(repository?.service_hosts||[]).map(entry=>entry.replace(':',' ')).join('\n');
  document.getElementById('git-dialog-title').textContent=repository?'Repository bewerken':'Repository toevoegen';
  document.getElementById('git-dialog-description').textContent=repository?'Wijzigingen gelden voor nieuwe Jobs; lopende en afgeronde Jobs houden hun bevroren Git-doel.':'De remote bepaalt de provider; credentials worden pas bij een actie voor de juiste gebruiker opgelost.';
  document.getElementById('git-submit').textContent=repository?'Wijzigingen bewaren':'Repository bewaren';
  form.elements.name.value=repository?.name||'';form.elements.remote_url.value=repository?.remote_url||'';form.elements.default_ref.value=repository?.default_ref||'main';form.elements.credential_scope.value=['user','global'].includes(repository?.credential_scope)?repository.credential_scope:'user';
  const selected=repository?.layer_selectors||[],available=[...new Set([...projectLayerSelectors(),...selected])];fillWithSelect(layers,selected,available);
}
function openGitEditor(id){const repository=byID(snapshot.git_repositories,id);if(!repository)return;resetGitForm(repository);openDialog('git-dialog');}

function renderGit(){
  const root=region('git-list');
  if(!snapshot.git_repositories.length){root.innerHTML='<div class="empty">Nog geen Git repositories. Maak eerst tool:git en voeg daarna een remote toe.</div>';return;}
  root.innerHTML=snapshot.git_repositories.map(repository=>{
    const scope=gitCredentialScope(repository.credential_scope,'public'),account=gitAccountForRepository(repository);
    const selectedLayers=repository.layer_selectors||[];
    const scopeLabel=scope==='user'?`user:${currentOperator()}`:scope;
    const access=scope==='public'?'<span class="tag">public · geen credentials</span>':account?`<span class="tag secret">${esc(scopeLabel)} → ${esc(gitAccountLabel(account))}</span>`:`<span class="tag warning">${esc(scopeLabel)} · ${esc(gitRemoteHost(repository.remote_url))} niet gekoppeld</span>`;
    const actions=repository.created_by===currentOperator()?`<div class="panel-actions repo-secondary"><button class="small-button" type="button" data-edit-git="${esc(repository.id)}">${icon('edit')}Bewerken</button><button class="danger" type="button" data-remove-git="${esc(repository.id)}">${icon('delete')}Verwijderen</button></div>`:'';
    return `<article class="repo-card"><div><div class="artifact-title"><strong>${esc(repository.name)}</strong><span class="tag branch">${esc(repository.provider||gitRemoteHost(repository.remote_url))}</span><span class="tag branch">${esc(repository.default_ref)}</span>${access}</div><small>${esc(repository.remote_url)}</small><small>Projectlagen: ${selectedLayers.map(esc).join(' · ')||'geen'}</small><small>Test-app: ${(repository.services||[]).length?repository.services.map(service=>`${esc(service.name)}${service.ports?.length?' :'+service.ports.map(esc).join('/'):''}`).join(' · '):'geen services'}${repository.service_hosts?.length?` · hosts: ${repository.service_hosts.map(esc).join(', ')}`:''}</small><small>Job: jobs/&lt;name-id&gt;/main · Sessions: jobs/&lt;name-id&gt;/sessions/&lt;id&gt;</small></div><div class="repo-controls">${actions}</div></article>`;
  }).join('');
  root.querySelectorAll('[data-edit-git]').forEach(button=>button.onclick=()=>openGitEditor(button.dataset.editGit));
  root.querySelectorAll('[data-remove-git]').forEach(button=>button.onclick=()=>removeGit(button.dataset.removeGit));
}

function renderGitAccounts(){
  const providers=document.getElementById('git-oauth-providers'),hint=document.getElementById('git-oauth-hint'),admin=authState.user?.role==='admin',accounts=[...myGitAccounts(),...globalGitAccounts()];
  providers.innerHTML=snapshot.git_oauth_providers.filter(provider=>provider.configured).map(provider=>`<button class="primary" type="button" data-oauth-provider="${esc(provider.id)}" data-oauth-scope="user">Koppel ${esc(provider.name)}</button>${admin?`<button class="small-button" type="button" data-oauth-provider="${esc(provider.id)}" data-oauth-scope="global">${esc(provider.name)} global</button>`:''}`).join('');
  hint.hidden=providers.children.length>0;
  hint.textContent=hint.hidden?'':'Configureer eerst onderaan een GitHub- of GitLab-OAuth application.';
  providers.querySelectorAll('[data-oauth-provider]').forEach(button=>button.onclick=()=>{location.href=`/api/git/oauth/${encodeURIComponent(button.dataset.oauthProvider)}/start?credential_scope=${encodeURIComponent(button.dataset.oauthScope)}`;});
  document.getElementById('git-credential-scope-field').hidden=!admin;
  if(!admin)document.getElementById('git-credential-scope').value='user';
  const configurations=document.getElementById('git-oauth-configurations');
  if(admin&&!snapshot.git_oauth_providers.some(provider=>provider.configured))document.getElementById('oauth-configuration-details').open=true;
  configurations.innerHTML=snapshot.git_oauth_providers.map(provider=>{
    const state=provider.configured?`<span class="tag capability">actief · ${esc(provider.source)}</span>`:'<span class="tag">niet ingesteld</span>';
    const instructions=`Maak bij <a href="${esc(provider.setup_url)}" target="_blank" rel="noopener noreferrer">${esc(provider.name)}</a> een OAuth application en gebruik exact deze callback: <code>${esc(provider.callback_url)}</code>`;
    if(!admin)return `<div class="section"><div class="artifact-title"><strong>${esc(provider.name)}</strong>${state}</div><p class="hint">${provider.configured?'Klaar om hierboven te verbinden.':'Een Spin-admin moet deze provider configureren.'}</p></div>`;
    if(provider.source==='environment')return `<div class="section"><div class="artifact-title"><strong>${esc(provider.name)}</strong>${state}</div><p class="hint">Beheerd via server environment · client ID ${esc(provider.client_id)}</p></div>`;
    return `<form class="form-grid oauth-config-form" data-provider="${esc(provider.id)}"><div class="wide"><div class="artifact-title"><strong>${esc(provider.name)}</strong>${state}</div><p class="hint" style="margin-top:7px">${instructions}</p></div><div><label>Client ID</label><input name="client_id" required value="${esc(provider.client_id||'')}" autocomplete="off"></div><div><label>Client secret${provider.configured?' (opnieuw invoeren om te wijzigen)':''}</label><input name="client_secret" type="password" required autocomplete="new-password"></div><div class="wide panel-actions"><button class="small-button" type="submit">${provider.configured?'Wijzig':'Configureer'} ${esc(provider.name)}</button>${provider.configured?`<button class="danger" type="button" data-remove-oauth="${esc(provider.id)}">Configuratie verwijderen</button>`:''}</div></form>`;
  }).join('');
  configurations.querySelectorAll('.oauth-config-form').forEach(form=>form.onsubmit=async event=>{event.preventDefault();const data=new FormData(form);try{await api(`/api/git/oauth/${encodeURIComponent(form.dataset.provider)}/configuration`,{method:'PUT',body:JSON.stringify({client_id:data.get('client_id'),client_secret:data.get('client_secret')})});form.reset();await refresh(true);}catch(error){showError(error);}});
  configurations.querySelectorAll('[data-remove-oauth]').forEach(button=>button.onclick=async()=>{if(!confirm('Deze OAuth application-configuratie verwijderen? Bestaande tokens blijven bestaan, maar kunnen mogelijk niet meer refreshen.'))return;try{await api(`/api/git/oauth/${encodeURIComponent(button.dataset.removeOauth)}/configuration`,{method:'DELETE'});await refresh(true);}catch(error){showError(error);}});
  const root=region('git-account-list');
  root.innerHTML=accounts.length?accounts.map(account=>{
    const scope=gitCredentialScope(account.credential_scope),expired=account.expires_at&&new Date(account.expires_at).getTime()<Date.now(),oauth=snapshot.git_oauth_providers.find(provider=>provider.id===account.provider&&provider.configured);
    // An expired OAuth token is refreshed on use; when that refresh is refused
    // (a rotated refresh token, typically after a restore) only a new
    // authorization helps, so that is the button on the card.
    const validity=expired?`<span class="tag warning">verlopen ${esc(new Date(account.expires_at).toLocaleString())} · vernieuwt bij gebruik, anders opnieuw koppelen</span>`:account.expires_at?'OAuth · verloopt '+esc(new Date(account.expires_at).toLocaleString()):'Token · geen vervaldatum';
    const reconnect=oauth&&(scope==='user'?account.operator===currentOperator():authState.user?.role==='admin')?`<button class="small-button" data-reconnect-git-account="${esc(account.provider)}" data-reconnect-scope="${esc(scope)}">Koppel opnieuw</button>`:'';
    return `<article class="git-account-card"><div><div class="artifact-title"><strong>${esc(account.provider)}:${esc(account.login)}</strong><span class="tag secret">${scope==='global'?'global':`user:${account.operator}`}</span></div><small>${esc(account.host)} · ${esc(account.name||account.login)}${account.email?' · '+esc(account.email):''}</small><small>${validity} · secret redacted</small></div><div class="panel-actions">${reconnect}${account.operator===currentOperator()?`<button class="danger" data-remove-git-account="${esc(account.id)}">Ontkoppel</button>`:''}</div></article>`;}).join(''):'<div class="empty">Nog geen persoonlijke of gedeelde Git identities.</div>';
  root.querySelectorAll('[data-remove-git-account]').forEach(button=>button.onclick=()=>removeGitAccount(button.dataset.removeGitAccount));
  root.querySelectorAll('[data-reconnect-git-account]').forEach(button=>button.onclick=()=>{location.href=`/api/git/oauth/${encodeURIComponent(button.dataset.reconnectGitAccount)}/start?credential_scope=${encodeURIComponent(button.dataset.reconnectScope)}`;});
}

function renderMCP(){
  const root=region('mcp-list'),servers=myMCP();
  if(!servers.length){root.innerHTML='<div class="empty">Nog geen persoonlijke MCP-configuraties.</div>';return;}
  root.innerHTML=servers.map(server=>`<article class="mcp-card"><div><div class="artifact-title"><span>${esc(server.name)}</span><span class="tag">${esc(server.transport)}</span><span class="tag secret">user:${esc(server.operator)}</span></div><small>${server.transport==='stdio'?esc(server.command)+' '+(server.args||[]).map(esc).join(' '):esc(server.url)}</small><small>${(server.env||[]).length} env credentials · ${(server.headers||[]).length} header credentials · values redacted</small></div><button class="danger" data-remove-mcp="${esc(server.id)}">Remove</button></article>`).join('');
  root.querySelectorAll('[data-remove-mcp]').forEach(button=>button.onclick=()=>removeMCP(button.dataset.removeMcp));
}

// The runner is a single binary per platform, taken from the release this
// server runs, so a downloaded client always matches the server.
let runnerToken='';
async function showRunnerToken(){try{const result=await api('/api/runners/token');runnerToken=result.token||'';renderRunnerToken();}catch(error){showError(error);}}
async function rotateRunnerToken(){if(!confirm('Nieuw worker-token maken? Alle runners moeten daarna met het nieuwe token opnieuw starten.'))return;try{const result=await api('/api/runners/token',{method:'POST'});runnerToken=result.token||'';renderRunnerToken();showNotice('Nieuw worker-token gemaakt; geef het aan de runners.');}catch(error){showError(error);}}
function renderRunnerToken(){
  const root=document.getElementById('runner-token');if(!root)return;
  if(authState.user?.role!=='admin'){root.innerHTML='';return;}
  root.innerHTML=runnerToken?`<code class="runner-token-value">${esc(runnerToken)}</code><button class="small-button" id="hide-runner-token">${icon('visibility_off')}Verberg</button><button class="small-button" id="rotate-runner-token">${icon('autorenew')}Vernieuw</button>`:`<button class="small-button" id="show-runner-token">${icon('key')}Toon worker-token</button>`;
  const show=document.getElementById('show-runner-token');if(show)show.onclick=showRunnerToken;
  const hide=document.getElementById('hide-runner-token');if(hide)hide.onclick=()=>{runnerToken='';renderRunnerToken();};
  const rotate=document.getElementById('rotate-runner-token');if(rotate)rotate.onclick=rotateRunnerToken;
}
function renderRunnerDownloads(){
  const root=region('runner-downloads');if(!root)return;
  const version=authState.version&&authState.version!=='dev'?authState.version:'',base=version?`https://github.com/xinix00/Spin/releases/download/${encodeURIComponent(version)}`:'https://github.com/xinix00/Spin/releases/latest/download';
  const builds=[['spin-client-darwin-arm64','macOS · Apple Silicon','laptop_mac'],['spin-client-linux-arm64','Linux · arm64','memory'],['spin-client-linux-amd64','Linux · amd64','memory']];
  root.innerHTML=`<div class="runner-download-row">${builds.map(([asset,label,glyph])=>`<a class="small-button" href="${base}/${asset}" download="${asset}">${icon(glyph)}${esc(label)}</a>`).join('')}<small>${version?`versie ${esc(version)}`:'laatste release'} · Docker Desktop of Docker Engine vereist</small></div><details class="runner-howto"><summary>Zo start je de runner</summary><pre>chmod +x spin-client-*
mkdir -p var && printf '%s' '&lt;worker-token&gt;' &gt; var/spin-worker.token
./spin-client-&lt;platform&gt; -server ${esc(location.origin)} -name $(hostname)</pre><div class="runner-token" id="runner-token"></div><small>Het worker-token hoort bij deze Spin en staat in zijn database; een admin ziet het hierboven en kan het vernieuwen. Opties: <code>-env-dir</code> voor de env-bestanden van de test-app, <code>-advertise-host</code> voor het adres in de test-app-links, <code>-max-workloads</code> voor het aantal gelijktijdige capsules.</small></details>`;
  renderRunnerToken();
}
// renderStorage shows what the server's database occupies; the volume under
// it is finite and Spin cannot see how much is left.
function replicationLine(replication){
  if(!replication)return `<span class="hint warning-text">${icon('cloud_off')} Geen replica: deze Spin staat alleen op zijn eigen volume.</span>`;
  const parts=[`${icon('cloud_done')} Replica in ${esc(replication.bucket||'S3')}`];
  parts.push(replication.complete?'generatie compleet':replication.generation?'snapshot loopt':'nog geen generatie');
  if(replication.last_sync_at&&!replication.last_sync_at.startsWith('0001'))parts.push(`laatste sync ${esc(elapsedSince(replication.last_sync_at))} geleden`);
  if(replication.pending_pages)parts.push(`${replication.pending_pages} pagina${replication.pending_pages===1?'':"'s"} wacht${replication.pending_pages===1?'':'en'}`);
  if(replication.restored)parts.push('bij het opstarten hersteld uit de replica');
  if(replication.last_error)parts.push(`fout: ${esc(replication.last_error)}`);
  return `<span class="hint ${replication.last_error?'warning-text':''}">${parts.join(' · ')}</span>`;
}
function renderStorage(){const root=region('storage-line'),storage=snapshot.storage||{};if(!root)return;if(storage.error){root.innerHTML=`<span class="hint">Opslag · ${esc(storage.error)}</span>`;return;}if(!storage.database_bytes){root.innerHTML='';return;}root.innerHTML=`<span class="hint">${icon('database')} Opslag op de server · database ${esc(formatBytes(storage.database_bytes))} · ${storage.objects} snapshots en bijlagen ${esc(formatBytes(storage.object_bytes))}${storage.prunable?` · ${storage.prunable} oude versie${storage.prunable===1?'':'s'} wacht op opruimen`:''}</span>${replicationLine(storage.replication)}`;}
function renderRunners(){
  renderRunnerDownloads();renderStorage();renderReplicaGenerations();
  const root=region('runner-list');
  if(!snapshot.clients.length){root.innerHTML='<div class="empty">Nog geen runner aangemeld. Start spin-client met de server-URL en het worker-token.</div>';return;}
  root.innerHTML=snapshot.clients.map(client=>{
    const engine=client.capabilities?.engine||{},pinned=snapshot.sessions.filter(session=>session.client_id===client.id),active=pinned.filter(session=>!['completed','cancelled'].includes(session.status));
    const statusClass=client.status==='online'?'capability':(client.draining||client.status==='draining'?'warning':''),admin=authState.user?.role==='admin',drainAction=admin?client.draining?`<button class="small-button" data-resume-client="${esc(client.id)}">${icon('play_arrow')}Hervat</button>`:`<button class="small-button" data-drain-client="${esc(client.id)}">${icon('pause')}Drain</button>`:'',removeAction=admin&&client.status==='offline'&&!active.length?`<button class="small-button" data-remove-client="${esc(client.id)}">${icon('delete')}Verwijder</button>`:'';
    return `<article class="mcp-card"><div><div class="artifact-title"><strong>${esc(client.name)}</strong><span class="tag ${statusClass}">${esc(client.status||'offline')}${client.draining?' · drained':''}</span><span class="tag">${esc(engine.driver||'unknown engine')}</span></div><small>${esc(client.capabilities?.os||'?')}/${esc(client.capabilities?.arch||'?')} · ${esc(client.id)} · max ${esc(client.capabilities?.max_workloads||'∞')} workloads</small><small>${active.length} actief · ${pinned.length} Sessions gekoppeld · laatst gezien ${esc(formatDateTime(client.last_seen_at))}${client.status==='offline'?` · ${esc(elapsedSince(client.last_seen_at))} geleden`:''}</small></div><div class="panel-actions">${drainAction}${removeAction}</div></article>`;
  }).join('');
  root.querySelectorAll('[data-drain-client]').forEach(button=>button.onclick=()=>setClientDraining(button.dataset.drainClient,true));
  root.querySelectorAll('[data-resume-client]').forEach(button=>button.onclick=()=>setClientDraining(button.dataset.resumeClient,false));
  root.querySelectorAll('[data-remove-client]').forEach(button=>button.onclick=async()=>{try{await api(`/api/clients/${encodeURIComponent(button.dataset.removeClient)}`,{method:'DELETE'});await refresh(true);}catch(error){showError(error);}});
}

function renderAccess(){
  const admin=authState.user?.role==='admin',form=region('user-form'),button=document.getElementById('open-user-dialog'),root=document.getElementById('user-list');
  form.hidden=!admin;button.hidden=!admin;document.getElementById('backup-panel').hidden=!admin;
  root.innerHTML=snapshot.users.length?snapshot.users.map(user=>{const archived=Boolean(user.archived_at),self=user.id===authState.user?.id,action=admin?`${archived?'':`<button class="small-button" data-password-user="${esc(user.id)}" data-user-label="${esc(user.display_name||user.username)}" title="Nieuw tijdelijk wachtwoord geven">${icon('key')}Wachtwoord</button>`}${self?'':archived?`<button class="small-button" data-restore-user="${esc(user.id)}">${icon('restore')}Herstel</button>`:`<button class="danger" data-archive-user="${esc(user.id)}">${icon('archive')}Archiveer</button>`}`:'';return `<article class="mcp-card ${archived?'archived':''}"><div><div class="artifact-title"><strong>${esc(user.display_name||user.username)}</strong><span class="tag ${user.role==='admin'?'secret':''}">${esc(user.role)}</span>${self?'<span class="tag capability">you</span>':''}${archived?'<span class="tag warning">gearchiveerd</span>':''}</div><small>@${esc(user.username)} · created ${esc(new Date(user.created_at).toLocaleDateString())}${archived?` · archived ${esc(formatDateTime(user.archived_at))}`:''}</small></div><div class="panel-actions">${action}</div></article>`;}).join(''):'<div class="empty">Nog geen gebruikers.</div>';
  root.querySelectorAll('[data-archive-user]').forEach(button=>button.onclick=()=>setUserArchived(button.dataset.archiveUser,true));
  root.querySelectorAll('[data-restore-user]').forEach(button=>button.onclick=()=>setUserArchived(button.dataset.restoreUser,false));
  root.querySelectorAll('[data-password-user]').forEach(button=>button.onclick=()=>{passwordUserID=button.dataset.passwordUser;document.getElementById('password-form').reset();document.getElementById('password-dialog-copy').textContent=`Nieuw tijdelijk wachtwoord voor ${button.dataset.userLabel}; alle sessies van deze gebruiker worden beëindigd.`;openDialog('password-dialog');requestAnimationFrame(()=>document.getElementById('password-new').focus());});
}
let passwordUserID='';
document.getElementById('password-form').onsubmit=async event=>{event.preventDefault();const form=new FormData(event.target);try{await api(`/api/auth/users/${encodeURIComponent(passwordUserID)}/password`,{method:'POST',body:JSON.stringify({password:form.get('password')})});closeDialog('password-dialog');showNotice(`Wachtwoord opnieuw ingesteld.${passwordUserID===authState.user?.id?' Je bent uitgelogd; log opnieuw in.':''}`);if(passwordUserID===authState.user?.id){stopStateStream();authState.authenticated=false;csrfToken='';showAuthGate('Je wachtwoord is gewijzigd. Log opnieuw in.');}else await refresh(true);}catch(error){showError(error);}};

async function backupResponse(path,options={}){
  const headers={...(options.headers||{})};if(csrfToken)headers['X-Spin-CSRF']=csrfToken;
  const response=await fetch(path,{...options,headers,credentials:'same-origin'});
  if(!response.ok){const body=await response.json().catch(()=>({error:response.statusText}));const error=new Error(body.error||response.statusText);error.status=response.status;throw error;}
  return response;
}
// A backup streams the live database straight into a browser download, as
// a zip with the portable key; Spin pauses its writes while it runs.
async function downloadPortableBackup(){
  const button=document.getElementById('download-backup'),label=button.innerHTML;button.disabled=true;
  try{const ticket=await (await backupResponse('/api/backup-ticket',{method:'POST'})).json(),link=document.createElement('a');link.href=ticket.url;link.download='';document.body.appendChild(link);link.click();link.remove();showNotice('Backup gestart · de browser toont de voortgang van de download; Spin pauzeert schrijfacties tot hij klaar is.');}
  catch(error){showError(error);}
  finally{setTimeout(()=>{button.disabled=false;button.innerHTML=label;},1500);}
}
function updateRestoreProgress(label,detail='',percentage=null,error=false,hide=false){
  const root=document.getElementById('restore-progress'),track=document.getElementById('restore-progress-track'),fill=document.getElementById('restore-progress-fill');root.hidden=hide;if(hide)return;root.classList.toggle('error',error);document.getElementById('restore-progress-label').textContent=label;document.getElementById('restore-progress-detail').textContent=detail;track.classList.toggle('indeterminate',percentage==null);if(percentage!=null)fill.style.width=`${Math.max(0,Math.min(100,percentage))}%`;
}
function updateRestoreStage(stage,message,current=0,total=0){
  const names={open:'Database openen',state:'State controleren',attachments:'Bijlagen controleren',snapshots:'Docker-lagen controleren',rollback:'Rollbackpunt maken',install:'Database activeren',secrets:'Credentials beveiligen',runners:'Runners opnieuw aanmelden'},percentage=total?current/total*100:null,detail=total?`${current}/${total}`:'';updateRestoreProgress(names[stage]||'Restore uitvoeren',message+(detail?` · ${detail}`:''),percentage);
}
function restoreProgressEvent(event){
  if(event.type==='error')throw new Error(event.error||'Restore mislukt');
  if(event.type==='complete'){updateRestoreProgress('Restore compleet','100%',100);return event.result;}
  updateRestoreStage(event.stage,event.message,event.current,event.total);return null;
}
function uploadRestoreChunk(uploadID,offset,chunk,onProgress){
  return new Promise((resolve,reject)=>{const xhr=new XMLHttpRequest();xhr.open('PUT',`/api/uploads/${encodeURIComponent(uploadID)}`);xhr.withCredentials=true;xhr.setRequestHeader('Content-Type','application/octet-stream');xhr.setRequestHeader('X-Spin-Upload-Offset',String(offset));if(csrfToken)xhr.setRequestHeader('X-Spin-CSRF',csrfToken);
    xhr.upload.onprogress=event=>onProgress(event.loaded);
    xhr.onerror=()=>reject(new Error('Verbinding verbroken tijdens restore-chunk'));xhr.onabort=()=>reject(new Error('Restore geannuleerd'));
    xhr.onload=()=>{let body={};try{body=JSON.parse(xhr.responseText||'{}');}catch(_){}if(xhr.status<200||xhr.status>=300){const error=new Error(body.error||xhr.statusText||'Restore-chunk geweigerd');error.status=xhr.status;error.offset=Number(body.offset);reject(error);return;}resolve(body);};xhr.send(chunk);
  });
}
function showRestoreUploadProgress(state){
  let sent=state.done;for(const loaded of state.inFlight.values())sent+=loaded;sent=Math.min(sent,state.size);const percentage=state.size?sent/state.size*100:0;updateRestoreProgress('Database uploaden',`${formatBytes(sent)} / ${formatBytes(state.size)} · ${Math.floor(percentage)}% · ${state.inFlight.size} chunks onderweg`,percentage);
}
// Every chunk is one round trip through the edge and the tunnel, so a few
// workers keep chunks in flight at once. The server stores them at their own
// offset and answers with the contiguous committed prefix.
async function restoreUploadWorker(upload,file,state){
  while(!state.error&&state.next<state.size){
    const offset=state.next,end=Math.min(offset+state.chunkSize,state.size);state.next=end;
    for(let attempt=1;!state.error;attempt++){
      state.inFlight.set(offset,0);
      try{
        const next=await uploadRestoreChunk(upload.id,offset,file.slice(offset,end),loaded=>{state.inFlight.set(offset,loaded);showRestoreUploadProgress(state);}),committed=Number(next.offset);
        if(!Number.isFinite(committed)||committed<0||committed>state.size)throw new Error('Spin gaf een ongeldige restore-offset terug');
        state.inFlight.delete(offset);state.done+=end-offset;state.committed=Math.max(state.committed,committed);showRestoreUploadProgress(state);break;
      }catch(error){
        state.inFlight.delete(offset);
        if(error.status===409&&error.offset>=end){state.done+=end-offset;state.committed=Math.max(state.committed,error.offset);break;}
        if(attempt>=5||error.status===404||error.status===413){state.error=error;return;}
        updateRestoreProgress('Uploadverbinding herstellen',`Poging ${attempt}/5 · chunk op ${formatBytes(offset)} opnieuw sturen…`,state.size?state.done/state.size*100:0);
        await new Promise(resolve=>setTimeout(resolve,Math.min(5000,500*2**(attempt-1))));
        try{const status=await (await backupResponse(`/api/uploads/${encodeURIComponent(upload.id)}`)).json();if(Number(status.size)!==state.size){state.error=new Error('Restore-uploadstatus komt niet overeen met het gekozen bestand');return;}}
        catch(statusError){if(statusError.status){state.error=statusError;return;}}
      }
    }
  }
}
const restoreJobStorageKey='spin-restore-job';
function rememberRestoreJob(id){try{sessionStorage.setItem(restoreJobStorageKey,id);}catch(_){}}
function forgetRestoreJob(){try{sessionStorage.removeItem(restoreJobStorageKey);}catch(_){}}
function rememberedRestoreJob(){try{return sessionStorage.getItem(restoreJobStorageKey)||'';}catch(_){return '';}}
function restoreJobProgress(job){
  if(job.status==='error'){const error=new Error(job.error||'Restore mislukt');error.restoreTerminal=true;throw error;}
  if(job.status==='complete'){if(!job.result)throw new Error('Restore eindigde zonder resultaat');updateRestoreProgress('Restore compleet','100%',100);return job.result;}
  updateRestoreStage(job.stage,job.message,job.current,job.total);return null;
}
async function pollRestoreJob(initial){
  let job=initial,failures=0;
  for(;;){
    let completed;try{completed=restoreJobProgress(job);}catch(error){if(error.restoreTerminal)forgetRestoreJob();throw error;}if(completed){forgetRestoreJob();return completed;}
    await new Promise(resolve=>setTimeout(resolve,Math.min(5000,1000+failures*500)));
    try{const response=await backupResponse(`/api/restores/${encodeURIComponent(job.id)}`);job=await response.json();failures=0;}
    catch(error){if(error.status===404){forgetRestoreJob();throw error;}failures+=1;updateRestoreProgress('Restore-status ophalen',`Verbinding herstellen · poging ${failures}`,null);if(failures>=12)throw new Error('Restore draait mogelijk nog op de server, maar de status is tijdelijk niet bereikbaar. Herlaad deze pagina om opnieuw te verbinden.');}
  }
}
async function completeRestoreUpload(uploadID){
  const response=await backupResponse(`/api/uploads/${encodeURIComponent(uploadID)}/complete`,{method:'POST'}),job=await response.json();if(!job.id)throw new Error('Spin gaf geen restore-status-ID terug');rememberRestoreJob(job.id);stopStateStream();return pollRestoreJob(job);
}
// Restore points from the replica thin out with age (quarter hours, hours,
// days, generations); putting one back is the same validated restore as a
// backup zip, without the upload.
async function renderReplicaGenerations(){
  const root=document.getElementById('replica-generations');if(!root)return;
  const replication=snapshot.storage?.replication;
  if(!replication||authState.user?.role!=='admin'){root.hidden=true;return;}
  root.hidden=false;
  if(!root.dataset.loaded){root.innerHTML=`<button class="small-button" id="load-generations">${icon('history')}Herstelpunten uit de replica</button>`;document.getElementById('load-generations').onclick=loadReplicaGenerations;}
}
const pointLevelLabel=level=>({'-1':'generatie',0:'sync',1:'kwartier',2:'uur',3:'dag'})[level]||`niveau ${level}`;
async function loadReplicaGenerations(){
  const root=document.getElementById('replica-generations');
  try{
    const result=await api('/api/replica/points'),points=result.points||[];root.dataset.loaded='1';
    root.innerHTML=points.length?`<p class="hint">${icon('history')} Herstelpunten in de replica, nieuwste eerst: elke sync van het laatste kwartier, kwartieren van de laatste twee uur, uren van de laatste dag, dagen van de laatste week en de wekelijkse generaties van de laatste maand. Een herstel vervangt de huidige database door de staat van dat moment.</p><div class="restore-points">${points.slice(0,400).map((point,index)=>`<div class="binding"><span>${esc(formatDateTime(point.at))}</span><span><span class="tag">${esc(pointLevelLabel(point.level))}</span>${index===0&&point.current?'<span class="tag capability">nu</span>':`<button class="small-button" data-restore-point="${index}" data-generation="${esc(point.generation)}" data-at="${esc(point.at)}" data-label="${esc(formatDateTime(point.at))}">${icon('restore')}Herstel</button>`}</span></div>`).join('')}</div>`:'<p class="hint">Nog geen complete generatie in de replica.</p>';
    root.querySelectorAll('[data-restore-point]').forEach(button=>button.onclick=()=>restoreReplicaPoint(button.dataset.generation,button.dataset.at,button.dataset.label));
  }catch(error){showError(error);}
}
async function restoreReplicaPoint(generation,at,label){
  if(!confirm(`Database terugzetten naar ${label}?\n\nAlles wat daarna is gebeurd verdwijnt uit deze Spin (de replica houdt de huidige staat nog als herstelpunt). Actieve browser-sessions worden afgesloten.`))return;
  updateRestoreProgress('Herstelpunt ophalen',label,0);
  try{const response=await backupResponse('/api/replica/restore',{method:'POST',body:JSON.stringify({generation,at})}),job=await response.json();if(!job.id)throw new Error('Spin gaf geen restore-status-ID terug');rememberRestoreJob(job.id);stopStateStream();const result=await pollRestoreJob(job);announceRestoreComplete(result);}
  catch(error){updateRestoreProgress('Herstel mislukt',error.message||String(error),100,true);showError(error);if(authState.authenticated&&stateStream.stopped)connectStateStream();}
}
function announceRestoreComplete(result){alert(`Restore compleet: ${result.jobs} Jobs, ${result.templates} Templates, ${result.deliverables} deliverables, ${result.attachments} bijlagen en ${result.snapshots} Docker-snapshots. Log opnieuw in.`);location.reload();}
async function uploadRestoreDatabase(file){
  const createdResponse=await backupResponse('/api/uploads',{method:'POST',body:JSON.stringify({kind:'restore',name:file.name,size:file.size})}),upload=await createdResponse.json(),offset=Number(upload.offset)||0;
  const state={size:file.size,chunkSize:Math.min(Number(upload.chunk_size)||1048576,1048576),next:offset,done:offset,committed:offset,inFlight:new Map(),error:null},parallel=Math.max(1,Math.min(Number(upload.parallel)||1,16));
  try{
    await Promise.all(Array.from({length:parallel},()=>restoreUploadWorker(upload,file,state)));
    if(state.error)throw state.error;
    if(state.committed!==file.size)throw new Error(`Spin bevestigde ${formatBytes(state.committed)} van ${formatBytes(file.size)}`);
    updateRestoreProgress('Upload compleet',`${formatBytes(file.size)} ontvangen · Spin opent de database…`,null);
    return await completeRestoreUpload(upload.id);
  }catch(error){
    await backupResponse(`/api/uploads/${encodeURIComponent(upload.id)}`,{method:'DELETE'}).catch(()=>{});
    throw error;
  }
}
async function restorePortableBackup(file){
  if(!file)return;if(!confirm(`Restore “${file.name}”?\n\nDe huidige server-state wordt vervangen. Actieve browser-sessions worden afgesloten; lopende runtime-handles worden niet meegenomen.`)){document.getElementById('restore-backup-input').value='';return;}
  const button=document.getElementById('choose-restore'),backupButton=document.getElementById('download-backup'),label=button.innerHTML;let restored=false;button.disabled=true;backupButton.disabled=true;button.innerHTML=`${icon('progress_activity')}Restore bezig…`;updateRestoreProgress('Upload starten',formatBytes(file.size),0);
  try{const result=await uploadRestoreDatabase(file);restored=true;announceRestoreComplete(result);}catch(error){updateRestoreProgress('Restore mislukt',error.message||String(error),100,true);showError(error);}finally{if(!restored&&!rememberedRestoreJob()&&authState.authenticated&&stateStream.stopped)connectStateStream();button.disabled=false;backupButton.disabled=false;button.innerHTML=label;document.getElementById('restore-backup-input').value='';}
}

function bindArtifactRemove(root){root.querySelectorAll('[data-remove-artifact]').forEach(button=>button.onclick=()=>removeArtifact(button.dataset.removeArtifact,button.dataset.artifactLabel));}
function bindSpawnForms(root){root.querySelectorAll('.spawn-form').forEach(form=>{
  const draft=spawnDrafts.get(form.dataset.jobId);
  if(draft){['objective_delta','role','environment_selector','spawned_by_session_id'].forEach(name=>{const input=form.elements[name];if(input&&draft[name]!=null)input.value=draft[name];});[...form.elements.with_selectors.options].forEach(option=>option.selected=(draft.with_selectors||[]).includes(option.value));[...form.elements.mcp_server_ids.options].forEach(option=>option.selected=(draft.mcp_server_ids||[]).includes(option.value));}
  const save=()=>spawnDrafts.set(form.dataset.jobId,{objective_delta:form.elements.objective_delta.value,role:form.elements.role.value,environment_selector:form.elements.environment_selector.value,with_selectors:selectedValues(form.elements.with_selectors),spawned_by_session_id:form.elements.spawned_by_session_id.value,mcp_server_ids:selectedValues(form.elements.mcp_server_ids)});
  form.oninput=save;form.onchange=save;
  form.onsubmit=async event=>{event.preventDefault();const data=new FormData(form);try{const created=await api(`/api/jobs/${encodeURIComponent(form.dataset.jobId)}/sessions`,{method:'POST',body:JSON.stringify({operator:currentOperator(),environment_selector:data.get('environment_selector'),with_selectors:selectedValues(form.querySelector('[name=with_selectors]')),mcp_server_ids:selectedValues(form.querySelector('[name=mcp_server_ids]')),objective_delta:data.get('objective_delta'),role:data.get('role'),spawned_by_session_id:data.get('spawned_by_session_id'),run:true})});if(created.run_error)showError(created.run_error);spawnDrafts.delete(form.dataset.jobId);await refresh(true);}catch(error){showError(error);}};
});}
function bindResultButtons(root){root.querySelectorAll('[data-select-result]').forEach(button=>button.onclick=async()=>{try{await api(`/api/jobs/${encodeURIComponent(button.dataset.jobId)}/select-result`,{method:'POST',body:JSON.stringify({result_id:button.dataset.selectResult})});await refresh(true);}catch(error){showError(error);}});}

async function removeArtifact(id,label){
  let members=[];try{members=(await api(`/api/artifacts/${encodeURIComponent(id)}/tree`)).members||[];}catch(error){showError(error);return;}
  const versions=members.filter(member=>member.version).length,above=members.filter(member=>!member.version);
  const lines=[`${label} verwijderen?`];
  if(versions)lines.push(`Alle ${versions+1} versies van deze laag gaan weg.`);
  if(above.length)lines.push(`De ${above.length} ${above.length===1?'laag':'lagen'} die erop gebouwd ${above.length===1?'is gaat':'zijn gaan'} mee, ook van andere gebruikers: ${above.map(member=>member.subject?`${member.layer} (${member.subject})`:member.layer).join(', ')}.`);
  lines.push('De snapshots verdwijnen uit archief en runners; dit kan niet ongedaan worden gemaakt.');
  if(!confirm(lines.join('\n\n')))return;
  try{await api(`/api/artifacts/${encodeURIComponent(id)}`,{method:'DELETE'});showNotice(`${label} verwijderd${members.length?` met ${members.length} ${members.length===1?'laag':'lagen'}`:''}`);await refresh(true);}catch(error){showError(error);}
}
async function removeMCP(id){if(!confirm('Remove deze persoonlijke MCP-configuratie?'))return;try{await api(`/api/mcp-servers/${encodeURIComponent(id)}`,{method:'DELETE'});await refresh(true);}catch(error){showError(error);}}
async function removeGitAccount(id){if(!confirm('Deze Git identity ontkoppelen? Repositories lossen daarna automatisch een andere identity voor dezelfde host en scope op, of wachten op een nieuwe koppeling.'))return;try{await api(`/api/git/accounts/${encodeURIComponent(id)}`,{method:'DELETE'});await refresh(true);}catch(error){showError(error);}}
async function removeGit(id){if(!confirm('Remove deze Git repositoryconfiguratie? Bestaande Jobs blokkeren dit.'))return;try{await api(`/api/git/repositories/${encodeURIComponent(id)}`,{method:'DELETE'});await refresh(true);}catch(error){showError(error);}}
async function removeTemplate(id){if(!confirm('Dit Workflow Template verwijderen? Bestaande Jobs blokkeren dit.'))return;try{await api(`/api/workflow-templates/${encodeURIComponent(id)}`,{method:'DELETE'});await refresh(true);}catch(error){showError(error);}}
async function setUserArchived(id,archived){const user=byID(snapshot.users,id);if(!user)return;if(archived&&!confirm(`Gebruiker “${user.display_name||user.username}” archiveren? Alle actieve logins worden ingetrokken; historie en gekoppelde records blijven bestaan.`))return;try{await api(`/api/auth/users/${encodeURIComponent(id)}/${archived?'archive':'restore'}`,{method:'POST'});await refresh(true);}catch(error){showError(error);}}
async function setClientDraining(id,draining){const client=byID(snapshot.clients,id);if(!client)return;try{await api(`/api/clients/${encodeURIComponent(id)}/${draining?'drain':'resume'}`,{method:'POST'});await refresh(true);}catch(error){showError(error);}}
async function closeJob(button){const id=button.dataset.closeJob,job=byID(snapshot.jobs,id);if(!job||!confirm(`Job “${job.title}” sluiten? De volledige historie blijft bewaard, maar actieve Sessions en capsules stoppen.`))return;button.disabled=true;const previous={status:job.status,workflow_status:job.workflow_status,current_phase_run_id:job.current_phase_run_id};job.status='cancelled';job.workflow_status='done';job.current_phase_run_id='';renderJobs();try{await api(`/api/jobs/${encodeURIComponent(id)}/close`,{method:'POST'});await refresh(true);}catch(error){Object.assign(job,previous);renderJobs();showError(error);}}
async function removeJob(id){const job=byID(snapshot.jobs,id);if(!job||!confirm(`Job “${job.title}” definitief verwijderen? Sessions en lokale containers verdwijnen; de remote Git-branches blijven bestaan.`))return;try{await api(`/api/jobs/${encodeURIComponent(id)}`,{method:'DELETE'});await refresh(true);}catch(error){showError(error);}}
async function retrySession(button){const id=button.dataset.retrySession;if(!id||!confirm('Deze Session opnieuw starten? Niet-gecommit werk in de huidige capsule wordt opgeruimd.'))return;const label=button.textContent;button.disabled=true;button.textContent='Retrying…';try{await api(`/api/sessions/${encodeURIComponent(id)}/retry`,{method:'POST'});await refresh(true);}catch(error){button.disabled=false;button.textContent=label;showError(error);}}

// The server pushes the state over one WebSocket: whole on connect, again
// on every change, and every few seconds while something moves in memory.
// refresh() stays for the moment right after an action; nothing polls.
const stateStream={socket:null,timer:null,stopped:true,failures:0};
function connectStateStream(){
  stateStream.stopped=false;clearTimeout(stateStream.timer);if(stateStream.socket&&[WebSocket.OPEN,WebSocket.CONNECTING].includes(stateStream.socket.readyState))return;
  const socket=new WebSocket(`${location.protocol==='https:'?'wss':'ws'}://${location.host}/api/state/ws`);stateStream.socket=socket;
  socket.onmessage=event=>{let next;try{next=JSON.parse(event.data);}catch(_){return;}stateStream.failures=0;applyState(next,false);};
  socket.onclose=()=>{if(stateStream.socket===socket)stateStream.socket=null;if(stateStream.stopped)return;document.getElementById('server-status').textContent='verbinding herstellen…';const delay=Math.min(15000,500*2**Math.min(stateStream.failures++,5));stateStream.timer=setTimeout(async()=>{if(stateStream.stopped)return;try{const status=await api('/api/auth/status');if(status?.authenticated){authState=status;csrfToken=status.csrf_token||csrfToken;}connectStateStream();}catch(error){if(error.status===401){stopStateStream();authState.authenticated=false;csrfToken='';showAuthGate('Je sessie is verlopen. Log opnieuw in.');return;}connectStateStream();}},delay);};
  socket.onerror=()=>socket.close();
}
function stopStateStream(){stateStream.stopped=true;clearTimeout(stateStream.timer);if(stateStream.socket){const socket=stateStream.socket;stateStream.socket=null;socket.close();}}
function applyState(next,force){
    ['artifacts','recordings','compositions','jobs','job_attachments','workflow_templates','phase_runs','deliverables','deliverable_comments','workflow_questions','sessions','activations','turns','checkpoints','results','clients','mcp_servers','git_repositories','git_accounts','git_oauth_providers','users'].forEach(key=>{if(!Array.isArray(next[key]))next[key]=[];});
    if(!force&&next.version&&snapshot.version&&next.version<snapshot.version)return;
    snapshot=next;
    if(next.current_user){authState.user=next.current_user;document.getElementById('current-user').textContent=`${next.current_user.display_name||next.current_user.username} · ${next.current_user.role}`;}
    // Background polling must not erase a half-written form.
    const editing=document.activeElement?.closest('form')&&document.activeElement?.id!=='command';
    if(force||!editing)render();
    const engine=snapshot.engine||{},onlineClients=snapshot.clients.filter(client=>['online','draining'].includes(client.status)).length,totalClients=snapshot.clients.length,drainingClients=snapshot.clients.filter(client=>client.draining).length,clientLabel=`${onlineClients}/${totalClients} client${totalClients===1?'':'s'} connected${drainingClients?` · ${drainingClients} draining`:''}`;
    document.getElementById('server-status').textContent=engine.driver?`${clientLabel} · ${engine.driver}`:clientLabel;
}
async function refresh(force=false){
  try{applyState(await api('/api/state'),force);}
  catch(error){document.getElementById('server-status').textContent='verbinding verbroken';if(error.status===401){stopStateStream();authState.authenticated=false;csrfToken='';showAuthGate('Je sessie is verlopen. Log opnieuw in.');}else showError(error);}
}

function setJobStep(step){
  document.querySelectorAll('[data-job-step]').forEach(panel=>panel.hidden=Number(panel.dataset.jobStep)!==step);
  document.querySelectorAll('[data-job-dot]').forEach(dot=>dot.classList.toggle('active',Number(dot.dataset.jobDot)<=step));
  document.getElementById('job-back').hidden=step===1;document.getElementById('job-next').hidden=step===2;document.getElementById('job-brainstorm').hidden=document.getElementById('job-submit').hidden=step!==2;
}

function newIdempotencyKey(){return globalThis.crypto?.randomUUID?.()||`${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;}
function setJobSubmitting(value,label=''){jobSubmitting=value;const button=document.getElementById('job-submit');button.textContent=value?(label||'Job wordt ingeschoten…'):'Job inschieten';renderEnvironmentOptions();}
function selectedJobAttachmentFiles(){return [...document.getElementById('job-attachments').files];}
function validateJobAttachmentFiles(files){const total=files.reduce((sum,file)=>sum+file.size,0);if(files.length>8)throw new Error('Kies maximaal 8 bijlagen.');if(files.some(file=>file.size>15*1024*1024))throw new Error('Een bijlage mag maximaal 15 MiB zijn.');if(total>40*1024*1024)throw new Error('Bijlagen mogen samen maximaal 40 MiB zijn.');}
function renderJobAttachmentSelection(){const files=selectedJobAttachmentFiles(),root=document.getElementById('job-attachment-selection');try{validateJobAttachmentFiles(files);root.innerHTML=files.map(file=>`<span class="attachment-chip">${icon(file.type==='application/pdf'?'picture_as_pdf':'image')}<span class="attachment-chip-name">${esc(file.name)}</span><span class="attachment-size">${esc(formatBytes(file.size))}</span></span>`).join('');}catch(error){root.innerHTML=`<span class="warning">${esc(error.message)}</span>`;}}
async function uploadAttachment(file,path){const body=new FormData();body.append('file',file,file.name);return api(path,{method:'POST',body});}
function chooseJobAttachments(jobID){attachmentTargetJobID=jobID;const input=document.getElementById('add-job-attachment-input');input.value='';input.click();}
async function addAttachmentsToJob(files){if(!attachmentTargetJobID||!files.length)return;try{validateJobAttachmentFiles(files);for(let index=0;index<files.length;index++){showNotice(`Bijlage ${index+1}/${files.length} uploaden naar Job…`);await uploadAttachment(files[index],`/api/jobs/${encodeURIComponent(attachmentTargetJobID)}/attachments`);}showNotice(`${files.length} bijlage${files.length===1?'':'n'} toegevoegd.`);await refresh(true);}catch(error){showError(error);await refresh(true);}finally{attachmentTargetJobID='';}}

function handleOAuthStatus(){
  const oauthStatus=new URLSearchParams(location.search).get('git_oauth');
  if(!oauthStatus)return false;
  setTab('connections');setConnection('git');
  if(oauthStatus!=='connected')showError(new Error(`Git OAuth: ${oauthStatus}`));
  history.replaceState({},'',location.pathname+'#connections');return true;
}

async function bootstrap(){
  const restoreID=rememberedRestoreJob();let restoreError=null;
  if(restoreID){
    updateRestoreProgress('Restore-status ophalen','Opnieuw verbinden met de server…',null);
    try{const response=await backupResponse(`/api/restores/${encodeURIComponent(restoreID)}`),result=await pollRestoreJob(await response.json());announceRestoreComplete(result);return;}
    catch(error){restoreError=error;if(rememberedRestoreJob()){showAuthGate(`Restore draait mogelijk nog: ${error.message||error}`);return;}}
  }
  try{
    const status=await api('/api/auth/status');authState=status;csrfToken=status.csrf_token||'';document.querySelectorAll('.brand-version').forEach(node=>{node.textContent=status.version&&status.version!=='dev'?status.version:'';node.title=status.version||'';});
    if(!status.configured){showAuthGate('Maak de eerste owner. Daarna bepaalt de server de operator voor iedere actie.','setup');return;}
    if(!status.authenticated){showAuthGate('Log in om jouw Snapshots, credentials en Git-identiteit te gebruiken.');return;}
    enterApp(status);
    setTab(localStorage.getItem('spin-tab')||'jobs');setWorkView(localStorage.getItem('spin-work-view')||'jobs');setJobState(jobStateFilter);setConnection(localStorage.getItem('spin-connection')||'git');handleOAuthStatus();
    await refresh(true);if(restoreError)showError(restoreError);
  }catch(error){
    if(error.status===503&&error.body?.stage){const started=error.body.started_at?Math.max(0,Math.round((Date.now()-new Date(error.body.started_at).getTime())/1000)):0;showAuthGate(error.body.opening?`${error.body.message}${started?` · ${started} s`:''}`:`Openen mislukt: ${error.body.failure||error.body.message}`);setGateBusy(!!error.body.opening);if(error.body.opening)setTimeout(bootstrap,2000);return;}
    // A redeploy or a proxy hiccup: keep asking until the server answers.
    showAuthGate(`Server niet bereikbaar: ${error.message||error} · opnieuw proberen…`);setGateBusy(true);setTimeout(bootstrap,3000);
  }
}

document.querySelectorAll('.tab-button').forEach(button=>button.onclick=()=>setTab(button.dataset.tab));
document.querySelectorAll('[data-connection]').forEach(button=>button.onclick=()=>setConnection(button.dataset.connection));
document.querySelectorAll('[data-work-view]').forEach(button=>button.onclick=()=>setWorkView(button.dataset.workView));
document.querySelectorAll('[data-job-state]').forEach(button=>button.onclick=()=>setJobState(button.dataset.jobState));
document.getElementById('job-search').oninput=event=>{jobSearch=event.target.value;renderJobs();};
document.querySelectorAll('[data-close-dialog]').forEach(button=>button.onclick=()=>closeDialog(button.dataset.closeDialog));
document.querySelectorAll('dialog').forEach(dialog=>dialog.addEventListener('click',event=>{if(event.target===dialog)dialog.close();}));
document.getElementById('deliverable-dialog').addEventListener('close',closeDeliverable);
document.getElementById('cancel-deliverable-comment').onclick=cancelDeliverableComment;
document.getElementById('deliverable-comment-form').onsubmit=async event=>{event.preventDefault();const selection=deliverableView.selection,item=byID(snapshot.deliverables,deliverableView.id),submit=event.submitter;if(!selection||!item)return;submit.disabled=true;try{const form=new FormData(event.target),comment=await api(`/api/deliverables/${encodeURIComponent(item.id)}/comments`,{method:'POST',body:JSON.stringify({...selection,body:form.get('body')})});snapshot.deliverable_comments.push(comment);renderDeliverableRevision(item.id);}catch(error){showError(error);await refresh(true);if(byID(snapshot.deliverables,item.id))renderDeliverableRevision(item.id);}finally{submit.disabled=false;}};
document.getElementById('job-changes-dialog').addEventListener('close',cancelCodeReviewComment);
document.getElementById('cancel-code-review-comment').onclick=cancelCodeReviewComment;
document.getElementById('code-review-comment-form').onsubmit=async event=>{event.preventDefault();const selection=jobChangesState.selection,revision=jobChangesState.bundle?.revision,submit=event.submitter;if(!selection||!revision)return;submit.disabled=true;try{const form=new FormData(event.target),comment=await api(`/api/code-reviews/${encodeURIComponent(revision.id)}/comments`,{method:'POST',body:JSON.stringify({...selection,body:form.get('body')})});jobChangesState.bundle.comments.push(comment);snapshot.code_review_comments.push(comment);cancelCodeReviewComment();renderCodeReviewComments(jobChangesState.bundle.comments);openJobChangeDiff(jobChangesState.selectedPath);}catch(error){showError(error);try{const stale=await api(`/api/code-reviews/${encodeURIComponent(revision.id)}`),latest=stale.latest_revision_id;if(latest&&latest!==revision.id)renderCodeReviewBundle(await api(`/api/code-reviews/${encodeURIComponent(latest)}`));else renderCodeReviewBundle(stale);}catch(_){}}finally{submit.disabled=false;}};
document.getElementById('chat-dialog').addEventListener('close',closeACPChat);
document.getElementById('chat-dialog').addEventListener('cancel',event=>{if(!document.getElementById('diff-drawer').hidden){event.preventDefault();closeDiff();}});
document.getElementById('close-diff').onclick=closeDiff;
document.getElementById('record-presets').innerHTML=recordPresets.map((preset,index)=>`<button class="quick" type="button" data-record-preset="${index}">${esc(preset.label)}</button>`).join('');
document.querySelectorAll('[data-record-preset]').forEach(button=>button.onclick=()=>fillRecordForm(recordPresets[Number(button.dataset.recordPreset)]));
document.getElementById('record-form').elements.enable_acp.onchange=syncRecordForm;
document.getElementById('record-form').onsubmit=event=>{event.preventDefault();const form=event.target;if(!form.reportValidity())return;const payload=recordRequestFromForm();closeDialog('layer-dialog');setTab('environments');openConsole();startLayerRecording(payload);};
document.getElementById('open-job-dialog').onclick=()=>{resetJobForm();openDialog('job-dialog');};
const openTemplateBuilder=()=>{resetTemplateForm();openDialog('template-dialog');};document.querySelectorAll('[data-open-template]').forEach(button=>button.onclick=openTemplateBuilder);document.getElementById('add-template-step').onclick=()=>addTemplateStep();
document.getElementById('job-next').onclick=()=>{const fields=[...document.querySelector('[data-job-step="1"]').querySelectorAll('input,textarea,select')];for(const field of fields){if(!field.checkValidity()){field.reportValidity();return;}}setJobStep(2);};
document.getElementById('job-back').onclick=()=>setJobStep(1);
['job-template','job-git','environment-selector'].forEach(id=>document.getElementById(id).addEventListener('change',updateJobRecipeCheck));
document.getElementById('job-attachments').onchange=renderJobAttachmentSelection;
document.getElementById('add-job-attachment-input').onchange=event=>addAttachmentsToJob([...event.target.files]);
document.getElementById('open-git-dialog').onclick=()=>{resetGitForm();openDialog('git-dialog');};
document.getElementById('open-mcp-dialog').onclick=()=>openDialog('mcp-dialog');
document.getElementById('open-user-dialog').onclick=()=>openDialog('user-dialog');
document.getElementById('download-backup').onclick=downloadPortableBackup;
document.getElementById('choose-restore').onclick=()=>document.getElementById('restore-backup-input').click();
document.getElementById('restore-backup-input').onchange=event=>restorePortableBackup(event.target.files[0]);
document.getElementById('explore-repository').onchange=event=>{if(event.target.value)exploreRepository(event.target.value);};
// The menu collapses to icons; the choice sticks. Explore starts collapsed
// unless the person opened the menu again: code wants the width.
function setNavCollapsed(collapsed){document.getElementById('app-shell').classList.toggle('nav-collapsed',collapsed);const toggle=document.getElementById('nav-toggle');toggle.querySelector('span').textContent=collapsed?'left_panel_open':'left_panel_close';toggle.title=collapsed?'Menu uitklappen':'Menu inklappen';try{localStorage.setItem('spin-nav',collapsed?'collapsed':'open');}catch(_){}requestAnimationFrame(()=>fitTerminal(activeTerminal()));}
document.getElementById('nav-toggle').onclick=()=>setNavCollapsed(!document.getElementById('app-shell').classList.contains('nav-collapsed'));
try{setNavCollapsed(localStorage.getItem('spin-nav')==='collapsed');}catch(_){}
document.getElementById('add-snapshot').onclick=()=>openLayerDialog({scope:'user'});
document.getElementById('record-git-tool').onclick=()=>openLayerDialog(recordPresets[0]);
document.getElementById('terminal-interrupt').onclick=interruptTerminal;
document.getElementById('capsule-dialog').addEventListener('cancel',event=>{const session=activeTerminal();if(session&&!session.exited&&document.activeElement?.closest('.pty-pane')){event.preventDefault();}});
// Refit only when the stage itself changed size, and never more than a few
// times a second: a fit that changed the terminal must not trigger itself.
let stageSize='';new ResizeObserver(entries=>{const box=entries[0]?.contentRect,key=box?`${Math.round(box.width)}x${Math.round(box.height)}`:'';if(key===stageSize)return;stageSize=key;clearTimeout(window._fitTimer);window._fitTimer=setTimeout(()=>fitTerminal(activeTerminal()),120);}).observe(document.getElementById('pty-stage'));
document.getElementById('chat-form').onsubmit=event=>{event.preventDefault();const input=document.getElementById('chat-input'),text=input.value.trim();if(!text)return;sendChat({type:'prompt',text});document.querySelectorAll('[data-chat-question]').forEach(node=>node.remove());input.value='';input.style.height='auto';setTimeout(()=>refresh(),250);};
document.getElementById('reject-form').onsubmit=event=>{event.preventDefault();if(!pendingRejectQuestionID)return;const form=new FormData(event.target);decideQuestion(pendingRejectQuestionID,'reject',String(form.get('reason')||'').trim());};
document.getElementById('chat-cancel').onclick=()=>sendChat({type:'cancel'});
const chatFeed=document.getElementById('chat-feed');
chatFeed.addEventListener('scroll',()=>{chatState.followTail=chatAtTail(chatFeed);},{passive:true});
if('ResizeObserver' in window)new ResizeObserver(()=>followChatTail()).observe(document.getElementById('chat-feed-inner'));
document.getElementById('chat-input').addEventListener('keydown',event=>{if(event.key==='Enter'&&!event.shiftKey){event.preventDefault();document.getElementById('chat-form').requestSubmit();}});
document.getElementById('chat-input').addEventListener('input',event=>{event.target.style.height='auto';event.target.style.height=`${Math.min(180,event.target.scrollHeight)}px`;});
document.getElementById('use-form').onsubmit=event=>{event.preventDefault();const selector=document.getElementById('use-selector').value,withSelectors=selectedValues(document.getElementById('use-with'));if(!selector)return;openConsole();useLayers(selector,withSelectors);};
document.getElementById('mcp-transport').onchange=event=>{const http=event.target.value==='http';document.getElementById('mcp-command-field').hidden=http;document.getElementById('mcp-url-field').hidden=!http;};
document.getElementById('git-provider').onchange=event=>{const hosts={github:'github.com',gitlab:'gitlab.com'};if(hosts[event.target.value])document.getElementById('git-host').value=hosts[event.target.value];};
// A brainstorm is a Job that starts with a chat instead of its first step;
// the goal comes out of that chat, so it may be empty here.
let jobBrainstorm=false;
document.getElementById('job-brainstorm').onclick=()=>{const form=document.getElementById('job-form'),objective=document.getElementById('objective');jobBrainstorm=true;objective.required=false;if(form.reportValidity())form.requestSubmit();else{jobBrainstorm=false;objective.required=true;}};
document.getElementById('job-form').onsubmit=async event=>{event.preventDefault();if(jobSubmitting)return;const brainstorm=jobBrainstorm;jobBrainstorm=false;document.getElementById('objective').required=true;const form=new FormData(event.target),files=selectedJobAttachmentFiles(),source=byID(snapshot.jobs,forkingJobID),payload={title:form.get('title'),reference:String(form.get('reference')||'').trim(),objective:form.get('objective'),brainstorm,owner:form.get('owner'),operator:currentOperator(),template_id:form.get('template_id'),git_repository_id:source?.git_repository_id||form.get('git_repository_id'),base_ref:source?.branch||form.get('base_ref'),environment_selector:form.get('environment_selector'),mcp_server_ids:selectedValues(document.getElementById('job-mcp')),forked_from_job_id:source?.id||'',run:true},fileFingerprint=files.map(file=>[file.name,file.size,file.type,file.lastModified]),fingerprint=JSON.stringify([payload,fileFingerprint]);setJobSubmitting(true);try{validateJobAttachmentFiles(files);if(!pendingJobSubmission||pendingJobSubmission.fingerprint!==fingerprint){if(pendingJobSubmission?.attachmentIds?.length)await Promise.allSettled(pendingJobSubmission.attachmentIds.map(id=>api(`/api/job-attachments/${encodeURIComponent(id)}`,{method:'DELETE'})));pendingJobSubmission={fingerprint,key:newIdempotencyKey(),attachmentIds:[]};}for(let index=pendingJobSubmission.attachmentIds.length;index<files.length;index++){setJobSubmitting(true,`Bijlage ${index+1}/${files.length} uploaden…`);const attachment=await uploadAttachment(files[index],'/api/job-attachments');pendingJobSubmission.attachmentIds.push(attachment.id);}payload.attachment_ids=[...pendingJobSubmission.attachmentIds];payload.idempotency_key=pendingJobSubmission.key;setJobSubmitting(true,'Job inschieten…');const created=await api('/api/jobs',{method:'POST',body:JSON.stringify(payload)});pendingJobSubmission=null;resetJobForm();closeDialog('job-dialog');setJobState('open');showNotice(brainstorm?'Brainstorm gestart; de omgeving wordt klaargezet.':`${source?'Vervolg-Job':'Job'} ingeschoten${files.length?` met ${files.length} bijlage${files.length===1?'':'n'}`:''}; de eerste Session start op de achtergrond.`);await refresh(true);
    // A brainstorm is the chat: open it right away and let it wait for the
    // capsule with the preparation in view.
    if(brainstorm&&created?.session?.id)openACPChat(created.session.id);}catch(error){showError(error);}finally{setJobSubmitting(false);}};
document.getElementById('template-form').onsubmit=async event=>{event.preventDefault();const form=new FormData(event.target),phases=collectTemplatePhases();if(!phases.length){showError(new Error('Voeg minimaal één stap toe.'));return;}const templateID=editingTemplateID,path=templateID?`/api/workflow-templates/${encodeURIComponent(templateID)}`:'/api/workflow-templates';try{await api(path,{method:templateID?'PUT':'POST',body:JSON.stringify({operator:currentOperator(),name:form.get('name'),description:form.get('description'),git_selector:form.get('git_selector'),phases})});resetTemplateForm();closeDialog('template-dialog');setWorkView('templates');await refresh(true);}catch(error){showError(error);}};
document.getElementById('git-account-form').onsubmit=async event=>{event.preventDefault();const form=new FormData(event.target);try{await api('/api/git/accounts',{method:'POST',body:JSON.stringify({operator:currentOperator(),provider:form.get('provider'),host:form.get('host'),login:form.get('login'),name:form.get('name'),email:form.get('email'),access_token:form.get('access_token'),credential_scope:form.get('credential_scope')||'user'})});event.target.reset();document.getElementById('git-provider').value='github';document.getElementById('git-host').value='github.com';document.getElementById('git-credential-scope').value='user';await refresh(true);}catch(error){showError(error);}};
document.getElementById('git-form').onsubmit=async event=>{event.preventDefault();const form=new FormData(event.target),repositoryID=editingGitRepositoryID,payload={operator:currentOperator(),name:form.get('name'),remote_url:form.get('remote_url'),default_ref:form.get('default_ref'),credential_scope:form.get('credential_scope'),layer_selectors:selectedValues(document.getElementById('git-layers')),services:collectServices(),service_hosts:String(form.get('service_hosts')||'').split('\n').map(line=>line.trim()).filter(Boolean)};try{await api(repositoryID?`/api/git/repositories/${encodeURIComponent(repositoryID)}`:'/api/git/repositories',{method:repositoryID?'PUT':'POST',body:JSON.stringify(payload)});resetGitForm();closeDialog('git-dialog');await refresh(true);}catch(error){showError(error);}};
document.getElementById('mcp-form').onsubmit=async event=>{event.preventDefault();const form=new FormData(event.target),transport=form.get('transport'),secretName=String(form.get('secret_name')||'').trim(),secretValue=String(form.get('secret_value')||'');const secret=secretName?[{name:secretName,value:secretValue}]:[];try{await api('/api/mcp-servers',{method:'POST',body:JSON.stringify({operator:currentOperator(),name:form.get('name'),transport,command:form.get('command'),args:String(form.get('args')||'').trim().split(/\s+/).filter(Boolean),url:form.get('url'),env:transport==='stdio'?secret:[],headers:transport==='http'?secret:[]})});event.target.reset();document.getElementById('mcp-command-field').hidden=false;document.getElementById('mcp-url-field').hidden=true;closeDialog('mcp-dialog');await refresh(true);}catch(error){showError(error);}};
document.getElementById('user-form').onsubmit=async event=>{event.preventDefault();const form=new FormData(event.target);try{await api('/api/auth/users',{method:'POST',body:JSON.stringify({username:form.get('username'),display_name:form.get('display_name'),role:form.get('role'),password:form.get('password')})});event.target.reset();document.getElementById('user-role').value='member';closeDialog('user-dialog');await refresh(true);}catch(error){showError(error);}};
document.getElementById('setup-form').onsubmit=async event=>{event.preventDefault();const form=new FormData(event.target);try{const status=await api('/api/auth/setup',{method:'POST',body:JSON.stringify({username:form.get('username'),display_name:form.get('display_name'),password:form.get('password')})});enterApp(status);setTab('connections');setConnection('git');showNotice('Owner created. Configureer nu Git OAuth of voeg een token-account toe.');await refresh(true);}catch(error){document.getElementById('auth-copy').textContent=error.message||error;}};
document.getElementById('login-form').onsubmit=async event=>{event.preventDefault();const form=new FormData(event.target);try{const status=await api('/api/auth/login',{method:'POST',body:JSON.stringify({username:form.get('username'),password:form.get('password')})});event.target.reset();enterApp(status);setTab(localStorage.getItem('spin-tab')||'jobs');setWorkView(localStorage.getItem('spin-work-view')||'jobs');setJobState(jobStateFilter);setConnection(localStorage.getItem('spin-connection')||'git');handleOAuthStatus();await refresh(true);}catch(error){document.getElementById('auth-copy').textContent=error.message||error;}};
document.getElementById('logout').onclick=async()=>{try{await api('/api/auth/logout',{method:'POST'});}catch(_){}closeACPChat();terminalSessions.forEach(session=>session.socket.close());terminalSessions.clear();stopStateStream();authState={configured:true,authenticated:false,user:null};csrfToken='';showAuthGate('Je bent uitgelogd.');};

bindActionButtons();bootstrap();
