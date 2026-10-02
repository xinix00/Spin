/* Editable suggestions for input[list]. The original input remains the form value. */
(() => {
  'use strict';
  const instances = new Map(); let sequence = 0;
  function mount(input) {
    if (instances.has(input)) return;
    const listID = input.getAttribute('list'); if (!listID) return;
    const wrapper = document.createElement('span'); wrapper.className = 't-input-action t-combobox';
    const arrow = document.createElement('button'); arrow.type = 'button'; arrow.className = 't-action t-icon-button'; arrow.tabIndex = -1;
    arrow.setAttribute('aria-label', 'Suggesties tonen'); arrow.innerHTML = '<span class="material-symbols-outlined t-icon" aria-hidden="true">expand_more</span>';
    const popup = document.createElement('div'); popup.className = 't-select-popup t-combobox-popup'; popup.popover = 'auto'; popup.id = `t-suggest-${++sequence}`; popup.setAttribute('role','listbox');
    popup.setAttribute('aria-label', input.labels?.[0]?.textContent.trim() || input.title || 'Suggesties');
    input.before(wrapper); wrapper.append(input,arrow,popup);
    const original = new Map(['list','role','aria-autocomplete','aria-controls','aria-expanded','aria-activedescendant'].map(name=>[name,input.getAttribute(name)]));
    input.removeAttribute('list'); input.setAttribute('role','combobox'); input.setAttribute('aria-autocomplete','list'); input.setAttribute('aria-controls',popup.id); input.setAttribute('aria-expanded','false');
    let values = [], index = -1, choosing = false;
    const isOpen = () => popup.matches(':popover-open');
    function sync() { arrow.disabled = input.matches(':disabled') || input.readOnly; }
    function position() {
      const r=wrapper.getBoundingClientRect(), spaceBelow=innerHeight-r.bottom-12, spaceAbove=r.top-12;
      popup.style.width=`${Math.min(r.width,innerWidth-24)}px`;
      popup.style.maxHeight=`${Math.max(80,Math.min(280,Math.max(spaceBelow,spaceAbove)))}px`;
      const height=popup.getBoundingClientRect().height;
      popup.style.left=`${Math.max(12,Math.min(r.left,innerWidth-popup.offsetWidth-12))}px`;
      popup.style.top=`${Math.max(12,spaceBelow>=height||spaceBelow>=spaceAbove?r.bottom+6:r.top-height-6)}px`;
    }
    function highlight(next) {
      index=next;
      [...popup.querySelectorAll('[role=option]')].forEach((row,n)=>{row.dataset.active=String(n===index);row.setAttribute('aria-selected',String(n===index));});
      const row=popup.querySelectorAll('[role=option]')[index];
      if(row){input.setAttribute('aria-activedescendant',row.id);row.scrollIntoView({block:'nearest'});}else input.removeAttribute('aria-activedescendant');
    }
    function choose(n) {
      if(!values[n])return; input.value=values[n].value; hide();
      choosing=true;
      try { input.dispatchEvent(new Event('input',{bubbles:true}));input.dispatchEvent(new Event('change',{bubbles:true})); } finally { choosing=false; hide(); }
      if (input.isConnected) input.focus({preventScroll:true});
    }
    function render() {
      const list=document.getElementById(listID),query=input.value.toLowerCase();
      values=[...(list?.options||[])].filter(option=>!option.disabled&&(option.value+' '+option.label).toLowerCase().includes(query)).map(option=>({value:option.value,label:option.label||option.value})).slice(0,100);
      popup.replaceChildren(); index=-1;input.removeAttribute('aria-activedescendant');
      values.forEach((value,n)=>{const row=document.createElement('div');row.className='t-select-option t-choice t-choice--marked';row.id=`${popup.id}-${n}`;row.setAttribute('role','option');row.setAttribute('aria-selected','false');row.textContent=value.label;row.addEventListener('pointerdown',event=>event.preventDefault());row.addEventListener('click',()=>choose(n));popup.append(row);});
      if(!values.length){const empty=document.createElement('div');empty.className='t-combobox-empty';empty.textContent='Geen suggesties. Je kunt een eigen waarde invoeren.';popup.append(empty);}
    }
    function show() { sync();if(arrow.disabled)return;render();if(!isOpen())popup.showPopover();input.setAttribute('aria-expanded','true');arrow.setAttribute('aria-expanded','true');position(); }
    function hide() { if(isOpen())popup.hidePopover(); input.setAttribute('aria-expanded','false');input.removeAttribute('aria-activedescendant');arrow.setAttribute('aria-expanded','false'); }
    const onInput=()=>{if(!choosing&&document.activeElement===input)show();};
    const onKey=event=>{
      if(event.isComposing)return;
      if(event.key==='ArrowDown'||event.key==='ArrowUp'){event.preventDefault();if(!isOpen())show();highlight(Math.max(0,Math.min(values.length-1,index+(event.key==='ArrowDown'?1:-1))));}
      else if(event.key==='Enter'&&isOpen()&&index>=0){event.preventDefault();event.stopPropagation();choose(index);}
      else if(event.key==='Escape'&&isOpen()){event.preventDefault();event.stopPropagation();hide();}
      else if(event.key==='Tab')hide();
    };
    const onBlur=()=>queueMicrotask(()=>{if(!wrapper.contains(document.activeElement))hide();});
    arrow.addEventListener('pointerdown',event=>event.preventDefault());arrow.addEventListener('click',()=>{if(isOpen())hide();else{input.focus({preventScroll:true});show();}});
    input.addEventListener('input',onInput); input.addEventListener('keydown',onKey);input.addEventListener('blur',onBlur);
    popup.addEventListener('toggle',()=>{if(!isOpen()){input.setAttribute('aria-expanded','false');arrow.setAttribute('aria-expanded','false');input.removeAttribute('aria-activedescendant');}});
    const onGeometry=()=>{if(isOpen())position();};window.addEventListener('resize',onGeometry);window.addEventListener('scroll',onGeometry,true);
    const localObserver=new MutationObserver(sync);localObserver.observe(input,{attributes:true,attributeFilter:['disabled','readonly']});sync();
    instances.set(input,{input,wrapper,popup,listID,refresh(){sync();if(isOpen()){render();position();}},destroy(){hide();localObserver.disconnect();input.removeEventListener('input',onInput);input.removeEventListener('keydown',onKey);input.removeEventListener('blur',onBlur);window.removeEventListener('resize',onGeometry);window.removeEventListener('scroll',onGeometry,true);for(const [name,value] of original){if(value===null)input.removeAttribute(name);else input.setAttribute(name,value);}if(input.parentElement===wrapper&&wrapper.parentNode){wrapper.before(input);wrapper.remove();}instances.delete(input);}});
  }
  function scan(root=document) {
    if(root.matches?.('input[list]'))mount(root);
    root.querySelectorAll?.('input[list]').forEach(mount);
  }
  function start() {
    scan();new MutationObserver(records=>{
      for(const record of records){for(const node of record.addedNodes)if(node.nodeType===1)scan(node);
        const list=record.target.closest?.('datalist');if(list)for(const item of instances.values())if(item.listID===list.id)item.refresh();
      }
      for(const item of instances.values())if(!item.input.isConnected)item.destroy();
    }).observe(document.documentElement,{childList:true,subtree:true});
  }
  globalThis.TactileCombobox=Object.freeze({mount:scan,get:input=>instances.get(input)});
  if(document.readyState==='loading')document.addEventListener('DOMContentLoaded',start,{once:true});else start();
})();
