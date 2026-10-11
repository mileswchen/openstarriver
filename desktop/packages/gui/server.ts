import { getRuntime } from "../tui/src/runtime.js";

const runtime = getRuntime();

function html(body: string, title = "Starriver") {
  return `<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>${title}</title><style>
*{margin:0;padding:0;box-sizing:border-box}
body{font:14px -apple-system,BlinkMacSystemFont,sans-serif;background:#1a1a2e;color:#e0e0e0;display:flex;min-height:100vh}
.sidebar{width:220px;background:#16213e;padding:16px;display:flex;flex-direction:column;gap:8px;flex-shrink:0}
.sidebar h2{color:#7c83ff;font-size:16px;margin-bottom:8px}
.sidebar a{color:#a0a0c0;text-decoration:none;padding:6px 8px;border-radius:4px;font-size:13px}
.sidebar a:hover,.sidebar a.active{background:#0f3460;color:#fff}
.main{flex:1;padding:24px;overflow-y:auto;min-width:0}
.card{background:#16213e;border-radius:6px;padding:16px;margin-bottom:12px}
.card h3{color:#7c83ff;margin-bottom:8px;font-size:15px}
.btn{padding:6px 14px;border-radius:4px;border:none;cursor:pointer;font-size:13px;margin-right:6px}
.btn-primary{background:#7c83ff;color:#fff}.btn-primary:hover{background:#6b72ee}
.btn-secondary{background:#0f3460;color:#e0e0e0}.btn-secondary:hover{background:#1a4a7a}
.btn-small{padding:3px 10px;font-size:12px}
.agent-row{display:flex;justify-content:space-between;align-items:center;padding:8px 0;border-bottom:1px solid #2a2a4a;flex-wrap:wrap;gap:4px}
.agent-row:last-child{border-bottom:none}
.badge{display:inline-block;padding:2px 8px;border-radius:10px;font-size:11px;margin-left:6px}
.badge-active{background:#2e7d32;color:#c8e6c9}
.badge-inactive{background:#555;color:#aaa}
.chat-log{background:#0d1117;border-radius:6px;padding:16px;min-height:300px;max-height:500px;overflow-y:auto;font:13px monospace;word-break:break-word}
.chat-log .user{color:#7c83ff;margin-bottom:4px}
.chat-log .agent{color:#58a6ff;margin-bottom:12px}
.chat-log .tool{color:#e2b714;margin-bottom:4px;font-size:12px}
.chat-log .ok{color:#4caf50;font-size:12px}
.chat-log .err{color:#f44336;font-size:12px}
.chat-input{display:flex;gap:8px;margin-top:12px}
.chat-input input{flex:1;padding:8px 12px;background:#0d1117;border:1px solid #30363d;border-radius:4px;color:#e0e0e0;font-size:13px}
@media(max-width:640px){body{flex-direction:column}.sidebar{width:100%;flex-direction:row;overflow-x:auto;padding:8px;gap:4px}.sidebar h2{font-size:14px;margin-right:8px;white-space:nowrap}.sidebar a{white-space:nowrap}.main{padding:12px}}
</style></head><body>${body}<div id="toast" style="position:fixed;bottom:20px;right:20px;background:#333;color:#fff;padding:8px 16px;border-radius:4px;font-size:12px;z-index:100;display:none"></div></body><script>
function toast(msg){const t=document.getElementById("toast");t.textContent=msg;t.style.display="block";setTimeout(()=>t.style.display="none",2000)}
// Chat SSE streaming
async function sendChat(){
  const inp=document.getElementById("chat-input");const log=document.getElementById("chat-log");
  if(!inp.value)return;const task=inp.value;inp.value="";
  log.innerHTML+='<div class="user">> '+task+'</div>';
  try{
    const res=await fetch("/api/execute",{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify({task})});
    if(!res.ok){log.innerHTML+='<div class="err">Error: '+res.status+'</div>';return}
    const reader=res.body.getReader();const decoder=new TextDecoder();
    let buf="";
    while(true){const{value,done}=await reader.read();if(done)break;
      buf+=decoder.decode(value,{stream:true});
      const lines=buf.split("\\n");buf=lines.pop()||"";
      for(const line of lines){
        if(!line.startsWith("data: "))continue;
        try{const ev=JSON.parse(line.slice(6));
          switch(ev.type||ev.eventType||ev.event_type||""){
            case"TaskStarted":break;
            case"TextDelta":log.innerHTML+='<span class="agent">'+(ev.text||ev.delta||"")+'</span>';break;
            case"ToolCallStart":log.innerHTML+='<div class="tool">🔧 '+(ev.name||ev.tool_name||"tool")+'</div>';break;
            case"ToolCallResult":log.innerHTML+='<div class="'+(ev.ok?"ok":"err")+'">'+(ev.ok?"✓":"✗")+'</div>';break;
            case"ToolCallError":log.innerHTML+='<div class="err">✗ '+(ev.error||"")+'</div>';break;
            case"TaskCompleted":log.innerHTML+='<div class="ok">✓ Done</div>';break;
            case"TaskFailed":log.innerHTML+='<div class="err">✗ '+(ev.error||"failed")+'</div>';break;
            default:log.innerHTML+='<span class="agent">'+JSON.stringify(ev)+'</span>';
          }
        }catch(e){}
      }
    }
  }catch(e){log.innerHTML+='<div class="err">Error: '+e.message+'</div>'}
  log.scrollTop=log.scrollHeight;
}
</script></html>`;
}

function sidebar(page: string) {
  return `<div class="sidebar">
    <h2>Starriver</h2>
    <a href="/" class="${page==="hub"?"active":""}">Hub</a>
    <a href="/chat" class="${page==="chat"?"active":""}">Chat</a>
    <a href="/skills" class="${page==="skills"?"active":""}">Skills</a>
    <a href="/settings" class="${page==="settings"?"active":""}">Settings</a>
  </div>`;
}

function hubPage() {
  let agents = runtime.listAgents();
  if (!agents.length) { runtime.scanAgents(); agents = runtime.listAgents(); }
  const cur = runtime.getCurrentAgent()?.name;
  const rows = agents.map(a => `<div class="agent-row">
    <span><strong>${a.name}</strong><span class="badge ${a.name===cur?"badge-active":"badge-inactive"}">${a.name===cur?"active":""}</span></span>
    <span style="font-size:12px;color:#888">${a.description}·v${a.version}</span>
    <form method="POST" action="/switch-agent" onsubmit="fetch('/api/switch-agent',{method:'POST',body:new URLSearchParams(new FormData(this))}).then(()=>location.reload());return false">
      <input type="hidden" name="name" value="${a.name}">
      <button class="btn btn-primary btn-small" ${a.name===cur?"disabled":""}>Switch</button>
    </form></div>`).join("");
  return `<div class="main"><h2 style="margin-bottom:16px">Agent Hub</h2>
    <div class="card"><h3>Current: ${cur||"(none)"}</h3><a href="/scan-agents" class="btn btn-secondary">Rescan</a></div>
    <div class="card"><h3>Agents (${agents.length})</h3>${rows||"<p>No agents yet.</p>"}</div></div>`;
}

function chatPage() {
  const cur = runtime.getCurrentAgent();
  return `<div class="main"><h2 style="margin-bottom:16px">Chat — ${cur?.name||"(none)"}</h2>
    <div class="card"><div class="chat-log" id="chat-log"></div>
    <div class="chat-input"><input id="chat-input" placeholder="Type a task..." onkeydown="if(event.key==='Enter')sendChat()"><button class="btn btn-primary" onclick="sendChat()">Send</button></div></div></div>`;
}

function skillsPage() {
  let skills = runtime.listSkills();
  if (!skills.length) { runtime.scanSkills(); skills = runtime.listSkills(); }
  const rows = skills.map(s => `<div class="agent-row"><strong>${s.name}</strong><span style="font-size:12px;color:#888">${s.description}${s.hasCommand?"·slash":""}</span></div>`).join("");
  return `<div class="main"><h2 style="margin-bottom:16px">Skills</h2>
    <div class="card"><h3>Skills (${skills.length})</h3><a href="/scan-skills" class="btn btn-secondary">Rescan</a></div>
    <div class="card">${rows||"<p>No skills yet.</p>"}</div></div>`;
}


function settingsPage() {
  const providers = runtime.listModelProviders?.() ?? [];
  const enabled = providers.filter(p => p.enabled);
  const rows = providers.map(p => `<div class="agent-row"><strong>${p.name}</strong><span class="badge ${p.enabled?"badge-active":"badge-inactive"}">${p.enabled?"enabled":"disabled"}</span><span style="font-size:12px;color:#888">${p.provider_type} · ${p.models.slice(0,3).join(", ")}</span></div>`).join("");
  return `<div class="main"><h2 style="margin-bottom:16px">CC Switch — Model Providers</h2>
    <div class="card"><h3>Cloud Providers (${enabled.length} enabled)</h3>${rows||"<p>No providers available.</p>"}</div>
    <div class="card"><h3>Add Provider</h3>
      <p style="font-size:12px;color:#888;margin-bottom:8px">Configure cloud API keys to use DeepSeek, QWen, GLM, Moonshot models</p>
      <form id="add-provider-form" onsubmit="addProvider();return false" style="display:flex;gap:8px;flex-wrap:wrap">
        <select id="prov-id"><option value="deepseek">DeepSeek</option><option value="qwen">QWen</option><option value="zhipu">GLM</option><option value="moonshot">Moonshot</option></select>
        <input id="prov-key" placeholder="API Key" type="password" style="flex:1;padding:6px;background:#0d1117;border:1px solid #30363d;color:#e0e0e0;border-radius:4px">
        <button class="btn btn-primary">Add</button>
      </form></div>
      <script>async function addProvider(){const id=document.getElementById("prov-id").value;const key=document.getElementById("prov-key").value;if(!key){alert("API Key required");return};await fetch("/api/model-providers",{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify({id,api_key:key})});location.reload()}</script></div>`;
}
const server = Bun.serve({
  port: 5173,
  async fetch(req) {
    const url = new URL(req.url);
    const p = url.pathname;
    try {
      // REST API
      if (req.method === "GET" && p === "/api/agents") return Response.json(runtime.listAgents());
      if (req.method === "GET" && p === "/api/skills") return Response.json(runtime.listSkills());
      if (req.method === "GET" && p === "/api/current-agent") return Response.json(runtime.getCurrentAgent()||{});

      // CC Switch — model providers
      if (req.method === "GET" && p === "/api/model-providers") return Response.json(runtime.listModelProviders?.() ?? []);
      if (req.method === "POST" && p === "/api/model-providers") {
        const { id, api_key, base_url } = await req.json();
        runtime.addCloudProvider?.(id, api_key, base_url);
        return Response.json({ok:true});
      }
      if (req.method === "DELETE" && p.startsWith("/api/model-providers/")) {
        const id = p.split("/").pop()!;
        runtime.removeCloudProvider?.(id);
        return Response.json({ok:true});
      }
      if (req.method === "POST" && p === "/api/switch-agent") {
        const fd = await req.formData();
        runtime.switchAgent(fd.get("name")!.toString());
        return Response.json({ok:true});
      }
      // SSE: execute agent task and stream events
      if (req.method === "POST" && p === "/api/execute") {
        const { task } = await req.json();
        if (!task) return Response.json({error:"task required"},{status:400});
        const stream = new ReadableStream({
          start(controller) {
            const encoder = new TextEncoder();
            controller.enqueue(encoder.encode(`data: ${JSON.stringify({type:"TaskStarted"})}\n\n`));
            let closed = false;
            const closeNow = () => { if (!closed) { closed = true; controller.close(); } };
            const failWith = (error: string) => {
              if (closed) return;
              closed = true;
              controller.enqueue(encoder.encode(`data: ${JSON.stringify({ type: "TaskFailed", error })}\n\n`));
              controller.close();
            };
            try {
              runtime.execute(task, (err: unknown, event: unknown) => {
                if (closed) return;
                if (err || !event) {
                  // Error-only callbacks must still terminate the stream —
                  // swallowing them leaves the browser reader pending forever.
                  failWith(String(err ?? "runtime error"));
                  return;
                }
                let payload: any = event;
                if (typeof event === 'string') {
                  try { payload = JSON.parse(event); } catch { failWith("malformed event frame"); return; }
                }
                controller.enqueue(encoder.encode(`data: ${JSON.stringify(payload)}\n\n`));
                const et = payload.type || payload.event_type || payload.eventType || '';
                if (et === 'TaskCompleted' || et === 'TaskFailed' || et === 'TaskCancelled') {
                  closeNow();
                }
              });
            } catch (e) {
              failWith(String(e));
            }
          }
        });
        return new Response(stream, {
          headers: { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', 'Connection': 'keep-alive' }
        });
      }
      // HTML pages
      if (p === "/") return new Response(html(sidebar("hub")+hubPage()));
      if (p === "/chat") return new Response(html(sidebar("chat")+chatPage()));
      if (p === "/skills") return new Response(html(sidebar("skills")+skillsPage()));
      if (p === "/scan-agents") { runtime.scanAgents(); return Response.redirect("/"); }
      if (p === "/scan-skills") { runtime.scanSkills(); return Response.redirect("/skills"); }
      if (req.method === "POST" && p === "/switch-agent") {
        const fd = await req.formData();
        runtime.switchAgent(fd.get("name")!.toString());
        return Response.redirect("/");
      }
      return new Response("Not Found", { status: 404 });
    } catch (e) { return new Response(`Error: ${e}`, { status: 500 }); }
  },
});
console.log(`Starriver Desktop GUI → http://localhost:${server.port}`);
await new Promise(() => {});
