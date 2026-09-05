//! 注入页面的一次性脚本:快照(元素枚举+ref)、elementInfo、虚拟剪贴板粘贴。
//!
//! 快照脚本移植自 ZCode `out/main/index.js` 的 `Rj()` 生成器(ACTION_SEL/
//! DOM_SEL/ref 编号 `e1..eN`/`__zcodeRefs` Map + WeakMap 双索引),输出形状
//! 与 ZCode `snapshot` 命令一致;坐标点选的动态 ref 走 `p<seq>`(`__zcodePtSeq`)。

/// ZCode 同款可交互元素选择器。
pub const ACTION_SEL: &str = "a[href], button, input, textarea, select, [role], [onclick], [tabindex], summary, label, [contenteditable]";
pub const DOM_SEL: &str = "body, main, nav, header, footer, aside, section, article, h1, h2, h3, h4, h5, h6, p, ul, ol, li, dl, dt, dd, blockquote, pre, code, table, caption, thead, tbody, tfoot, tr, th, td, form, fieldset, legend, figure, figcaption, img, canvas, svg, a[href], button, input, textarea, select, option, summary, label, [role], [aria-label], [contenteditable]";

/// 快照脚本参数:max(可交互元素上限), domMax(DOM 上限), includeHidden。
pub fn snapshot_js(max: u32, dom_max: u32, include_hidden: bool) -> String {
    format!(
        r#"(function(){{
var MAX={max};var DOM_MAX={dom_max};var INCLUDE_HIDDEN={include_hidden};
var ACTION_SEL={action_sel};var DOM_SEL={dom_sel};
function safeId(id){{return /^[A-Za-z][\w:-]*$/.test(id)?id:null;}}
function isHidden(el){{
  if(!el.isConnected)return true;
  var style;try{{style=window.getComputedStyle(el);}}catch(e){{return false;}}
  if(style.display==='none'||style.visibility==='hidden'||Number(style.opacity)===0)return true;
  var r=el.getBoundingClientRect();return r.width===0&&r.height===0;
}}
function cssPath(el){{
  if(!(el instanceof Element))return '';
  var path=[];while(el&&el.nodeType===Node.ELEMENT_NODE&&path.length<6){{
    var sel=el.tagName.toLowerCase();
    if(el.id&&safeId(el.id)){{sel+='#'+el.id;path.unshift(sel);break;}}
    var sib=el, nth=1;
    while((sib=sib.previousElementSibling)){{if(sib.tagName===el.tagName)nth++;}}
    sel+=':nth-of-type('+nth+')';path.unshift(sel);el=el.parentElement;
  }}
  return path.join(' > ');
}}
function xpathOf(el){{
  if(el.id&&safeId(el.id))return "//*[@id='"+el.id+"']";
  var parts=[];var cur=el;
  while(cur&&cur.nodeType===1){{
    var t=cur.tagName.toLowerCase();var idx=1;var sib=cur.previousElementSibling;
    while(sib){{if(sib.tagName===cur.tagName)idx++;sib=sib.previousElementSibling;}}
    parts.unshift(t+'['+idx+']');cur=cur.parentElement;
  }}
  return '/'+parts.join('/');
}}
try{{window.__zcodeRefs=new Map();}}catch(e){{window.__zcodeRefs=null;}}
try{{window.__zcodeRefMeta=new Map();}}catch(e){{window.__zcodeRefMeta=null;}}
var elRef=(typeof WeakMap!=='undefined')?new WeakMap():null;
var vw=window.innerWidth||document.documentElement.clientWidth||0;
var vh=window.innerHeight||document.documentElement.clientHeight||0;
var nodes=document.querySelectorAll(ACTION_SEL);
var elements=[];var truncated=false;var count=0;
for(var i=0;i<nodes.length;i++){{
  var el=nodes[i];
  if(!INCLUDE_HIDDEN&&isHidden(el))continue;
  if(count>=MAX){{truncated=true;break;}}
  count++;
  var ref='e'+count;
  if(window.__zcodeRefs)window.__zcodeRefs.set(ref,el);
  if(elRef)elRef.set(el,ref);
  var r=el.getBoundingClientRect();
  var rect={{x:Math.round(r.x),y:Math.round(r.y),width:Math.round(r.width),height:Math.round(r.height)}};
  var inViewport=r.top<vh&&r.bottom>0&&r.left<vw&&r.right>0;
  var tag=el.tagName.toLowerCase();
  var out={{ref:ref,tag:tag,selector:cssPath(el),xpath:xpathOf(el),rect:rect,inViewport:inViewport}};
  out.role=el.getAttribute('role')||'';
  out.name=(el.getAttribute('aria-label')||el.innerText||el.value||el.placeholder||el.getAttribute('title')||'').toString().slice(0,200);
  if(tag==='input'||tag==='textarea'){{out.editable=true;out.inputType=el.type||'';}}
  if(tag==='a'&&el.getAttribute('href'))out.href=el.href;
  if(tag==='select'){{out.options=Array.prototype.slice.call(el.options).slice(0,20).map(function(o){{return o.value;}});}}
  if(el.checked!==undefined)out.checked=!!el.checked;
  if(window.__zcodeRefMeta)window.__zcodeRefMeta.set(ref,{{selector:out.selector,xpath:out.xpath,role:out.role,name:out.name,tag:tag}});
  elements.push(out);
}}
var domCount=0;var domTruncated=false;
var domNodes=document.querySelectorAll(DOM_SEL);var dom=[];
for(var j=0;j<domNodes.length;j++){{
  if(domCount>=DOM_MAX){{domTruncated=true;break;}}
  var d=domNodes[j];
  if(!INCLUDE_HIDDEN&&isHidden(d))continue;
  domCount++;
  var txt=(d.innerText||'').trim();
  dom.push({{tag:d.tagName.toLowerCase(),text:txt.length>120?txt.slice(0,120)+'…':txt}});
}}
// 轻量 AX 树：优先使用显式 aria 语义，补充可见文本和层级，供模型稳定定位。
var ax=[];var axCount=0;
function axRole(el){{return el.getAttribute('role')||({{button:'button',a:'link',input:'textbox',textarea:'textbox',select:'combobox',img:'img',h1:'heading',h2:'heading',h3:'heading',h4:'heading',h5:'heading',h6:'heading'}}[el.tagName.toLowerCase()]||'generic');}}
function axWalk(el,depth){{if(!el||depth>8||axCount>=DOM_MAX)return;var hidden=isHidden(el);if(hidden&&!INCLUDE_HIDDEN)return;var role=axRole(el);var name=(el.getAttribute('aria-label')||el.getAttribute('alt')||el.innerText||el.value||'').toString().trim().replace(/\\s+/g,' ').slice(0,200);if(role!=='generic'||name){{ax.push({{role:role,name:name,level:depth,ref:(window.__zcodeRefMeta&&Array.from(window.__zcodeRefMeta.entries()).find(function(x){{return x[1].selector&&el.matches&&el.matches(x[1].selector);}})||[])[0]||null}});axCount++;}}Array.prototype.forEach.call(el.children||[],function(child){{axWalk(child,depth+1);}});}}
axWalk(document.body,0);
return JSON.stringify({{
  url:location.href,title:document.title,viewportWidth:vw,viewportHeight:vh,
  scrollX:window.scrollX||0,scrollY:window.scrollY||0,
  elements:elements,truncated:truncated,
  dom:dom,domTruncated:domTruncated,accessibility:ax
}});
}})()"#,
        max = max,
        dom_max = dom_max,
        include_hidden = include_hidden,
        action_sel = serde_json::to_string(ACTION_SEL).unwrap(),
        dom_sel = serde_json::to_string(DOM_SEL).unwrap(),
    )
}

/// 坐标处元素信息(点选 ref 编号 `p<seq>`),与 ZCode `__zcodePtSeq` 同款。
pub fn element_info_js(x: f64, y: f64) -> String {
    format!(
        r#"(function(){{
var x={x},y={y};
var el=document.elementFromPoint(x,y);
if(!el)return JSON.stringify({{found:false}});
if(!window.__zcodeRefs){{try{{window.__zcodeRefs=new Map();}}catch(e){{window.__zcodeRefs=null;}}}}
window.__zcodePtSeq=(window.__zcodePtSeq||0)+1;
var ref='p'+window.__zcodePtSeq;
if(window.__zcodeRefs)window.__zcodeRefs.set(ref,el);
var r=el.getBoundingClientRect();
var tag=el.tagName.toLowerCase();
var html=el.outerHTML||'';
return JSON.stringify({{
  found:true,ref:ref,tag:tag,
  selector:(function(){{if(el.id)return '#'+el.id;var s=tag;if(el.className&&typeof el.className==='string')s+='.'+el.className.trim().split(/\s+/).slice(0,2).join('.');return s;}})(),
  xpath:(function(){{if(el.id&&/^[A-Za-z][\w:-]*$/.test(el.id))return "//*[@id='"+el.id+"']";return '';}})(),
  rect:{{x:Math.round(r.x),y:Math.round(r.y),width:Math.round(r.width),height:Math.round(r.height)}},
  text:(el.innerText||'').slice(0,300),
  html:html.length>400?html.slice(0,400)+'…':html,
  value:(el.value!==undefined?String(el.value):undefined)
}});
}})()"#,
        x = x,
        y = y,
    )
}

/// 按 ref 解析元素中心坐标(CDP Runtime.evaluate 前置步骤)。
pub fn resolve_ref_center_js(ref_id: &str) -> String {
    format!(
        r#"(function(){{
var ref={ref_json};
var map=window.__zcodeRefs;
if(!map)return JSON.stringify({{found:false,reason:'no_snapshot'}});
var el=map.get(ref);
if(!el||!el.isConnected){{
  var meta=window.__zcodeRefMeta&&window.__zcodeRefMeta.get(ref), candidates=[];
  try{{if(meta&&meta.selector)candidates=Array.from(document.querySelectorAll(meta.selector));}}catch(e){{}}
  if(!candidates.length&&meta&&meta.xpath){{try{{var x=document.evaluate(meta.xpath,document,null,XPathResult.FIRST_ORDERED_NODE_TYPE,null).singleNodeValue;if(x)candidates=[x];}}catch(e){{}}}}
  if(meta&&candidates.length){{
    el=candidates.find(function(node){{return (!meta.role||node.getAttribute('role')===meta.role)&&(!meta.name||((node.getAttribute('aria-label')||node.innerText||node.value||node.placeholder||'').toString().slice(0,200)===meta.name));}})||candidates[0];
    if(window.__zcodeRefs)window.__zcodeRefs.set(ref,el);
  }}
}}
if(!el||!el.isConnected)return JSON.stringify({{found:false,reason:'stale_ref'}});
var r=el.getBoundingClientRect();
if(r.width===0&&r.height===0)return JSON.stringify({{found:false,reason:'hidden'}});
return JSON.stringify({{found:true,cx:r.x+r.width/2,cy:r.y+r.height/2}});
}})()"#,
        ref_json = serde_json::to_string(ref_id).unwrap(),
    )
}

/// 虚拟剪贴板粘贴:聚焦元素上派发 paste 事件写入整段文本。
/// 移植自 ZCode `pasteTextIntoFocusedTarget`(简化:只走纯文本)。
pub fn paste_text_js(text: &str) -> String {
    let payload = serde_json::to_string(text).unwrap();
    format!(
        r#"(async function(){{
var text={payload};
var el=document.activeElement;
// activeElement 不可编辑时向上找 contenteditable 祖先(B 站等站点输入框是
// 嵌套结构,焦点常落在外层 div 或内层占位元素上)。
if(el&&!(el.isContentEditable||el instanceof HTMLInputElement||el instanceof HTMLTextAreaElement)){{
  el=el.closest('[contenteditable="true"],[contenteditable=""],input,textarea')||el;
}}
if(!el||!(el instanceof HTMLElement))return JSON.stringify({{ok:false,error:'no_focused_input'}});
var dt;
try{{dt=new DataTransfer();}}catch(e){{return JSON.stringify({{ok:false,error:'no_data_transfer'}});}}
dt.setData('text/plain',text);
var event;
try{{event=new ClipboardEvent('paste',{{clipboardData:dt,bubbles:true,cancelable:true}});}}
catch(e){{
  event=document.createEvent('Event');
  event.initEvent('paste',true,true);
  event.clipboardData=dt;
}}
var prevented=!el.dispatchEvent(event);
if(!prevented){{
  // 未被页面拦截:对可编辑元素直接写入(与 beforeinput 兜底一致)
  if(el.isContentEditable){{el.textContent+=text;}}
  else if(el instanceof HTMLInputElement||el instanceof HTMLTextAreaElement){{
    var start=el.selectionStart??el.value.length;var end=el.selectionEnd??el.value.length;
    el.value=el.value.slice(0,start)+text+el.value.slice(end);
    var pos=start+text.length;el.setSelectionRange(pos,pos);
    el.dispatchEvent(new Event('input',{{bubbles:true}}));
  }}
  else return JSON.stringify({{ok:false,error:'target_not_editable'}});
}}
return JSON.stringify({{ok:true}});
}})()"#,
        payload = payload,
    )
}

/// waitFor 文本出现/消失的轮询片段。
pub fn wait_text_js(text: &str, _want_present: bool) -> String {
    format!(
        r#"(function(){{
var needle={text_json};
var present=document.body&&document.body.innerText.indexOf(needle)!==-1;
return JSON.stringify({{present:present}});
}})()"#,
        text_json = serde_json::to_string(text).unwrap(),
    )
}
