// Canvas and SVG renderers read the same palette as the CSS components.
function themeColor(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

// Shared rich-text rendering for chat, deliverables and source views.
const spinAssetBase = new URL('../', import.meta.url).href.replace(/\/$/, '');

const esc = value => String(value ?? '').replace(/[&<>'"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;',"'":'&#39;','"':'&quot;'}[c]));
const icon = name => `<span class="material-symbols-outlined t-icon" aria-hidden="true">${esc(name)}</span>`;

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
markdownRenderer.code=function({text,lang}){const language=syntaxLanguage(String(lang||'').split(/\s+/)[0]);if(language==='mermaid')return `<div class="mermaid-shell t-well"><div class="mermaid mermaid-loading" data-mermaid-pending="true">${esc(text)}</div></div>`;return sourceCode(text,language||lang||'');};
markdownRenderer.table=function(token){return `<div class="md-table-scroll t-table-scroll">${defaultTableRenderer.call(this,token).replace('<table>','<table class="t-table">')}</div>`;};
markdownRenderer.link=function(token){return defaultLinkRenderer.call(this,token).replace(/^<a /,'<a target="_blank" rel="noopener noreferrer" ');};
function markdown(value){
  const source=String(value||'').replace(/^[\u200B\u200C\u200D\u200E\u200F\uFEFF]/,'');try{return DOMPurify.sanitize(marked.parse(source,{gfm:true,breaks:true,renderer:markdownRenderer}),{USE_PROFILES:{html:true},ADD_ATTR:['target','rel'],FORBID_TAGS:['style','form','button','textarea','select','option'],FORBID_ATTR:['style'],SANITIZE_NAMED_PROPS:true});}catch(error){return `<pre class="syntax-code markdown-error"><code>${esc(source)}</code></pre>`;}
}
let mermaidLoader=null;
function loadMermaid(){
  if(window.mermaid)return Promise.resolve(window.mermaid);if(mermaidLoader)return mermaidLoader;mermaidLoader=new Promise((resolve,reject)=>{const script=document.createElement('script');script.src=`${spinAssetBase}/vendor/mermaid-11.17.2.min.js`;script.onload=()=>{if(!window.mermaid){reject(new Error('Mermaid library ontbreekt'));return;}window.mermaid.initialize({startOnLoad:false,securityLevel:'strict',theme:'dark',suppressErrorRendering:true,themeVariables:{background:themeColor('--input'),primaryColor:themeColor('--accent-soft'),primaryTextColor:themeColor('--text'),primaryBorderColor:themeColor('--accent-border'),lineColor:themeColor('--accent-ink'),secondaryColor:themeColor('--soft'),tertiaryColor:themeColor('--warning-surface'),fontFamily:themeColor('--sans')}});resolve(window.mermaid);};script.onerror=()=>reject(new Error('Mermaid kon niet worden geladen'));document.head.appendChild(script);});return mermaidLoader;
}
async function renderMermaid(root){
  const nodes=[...(root?.matches?.('.mermaid[data-mermaid-pending]')?[root]:[]),...(root?.querySelectorAll?.('.mermaid[data-mermaid-pending]')||[])];if(!nodes.length)return;nodes.forEach(node=>node.dataset.mermaidPending='rendering');try{const engine=await loadMermaid();for(const node of nodes){if(!node.isConnected)continue;const source=node.textContent||'';try{await engine.run({nodes:[node],suppressErrors:true});if(!node.querySelector('svg'))throw new Error('ongeldige Mermaid-syntax');node.classList.remove('mermaid-loading');node.removeAttribute('data-mermaid-pending');}catch(error){node.className='mermaid-error';node.removeAttribute('data-mermaid-pending');node.textContent=`Diagram kon niet worden gerenderd: ${error.message||error}\n\n${source}`;}}}catch(error){nodes.forEach(node=>{if(!node.isConnected)return;node.className='mermaid-error';node.removeAttribute('data-mermaid-pending');node.textContent=error.message||String(error);});}
}
function setMarkdown(root,value){root.innerHTML=markdown(value);return renderMermaid(root);}

export { themeColor, esc, icon, syntaxLanguage, highlightCode, sourceCode, markdown, renderMermaid, setMarkdown };
