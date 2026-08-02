import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { GraphEdge, GraphModel } from './graph';

interface VisualNode {
  readonly id: string;
  readonly label: string;
  readonly file: string;
  readonly location: string;
  readonly kind: string;
  readonly community: string;
  readonly communityName: string;
  readonly degree: number;
}

interface VisualEdge {
  readonly source: string;
  readonly target: string;
  readonly relation: string;
  readonly confidence: string;
}

export class GraphVisualizer implements vscode.Disposable {
  private panel?: vscode.WebviewPanel;
  private model?: GraphModel;
  private selectedCommunity?: string;
  private messageSubscription?: vscode.Disposable;

  constructor(
    private readonly extensionUri: vscode.Uri,
    private readonly onReveal: (id: string) => void,
    private readonly onExplain: (id: string) => void,
  ) {}

  show(model: GraphModel, selectedCommunity?: string, placement: 'active' | 'beside' = 'active'): void {
    this.model = model;
    this.selectedCommunity = selectedCommunity;
    const column = placement === 'beside'
      ? vscode.ViewColumn.Beside
      : vscode.window.activeTextEditor?.viewColumn ?? this.panel?.viewColumn ?? vscode.ViewColumn.One;
    if (!this.panel) {
      this.panel = vscode.window.createWebviewPanel(
        'graphoxide.graph',
        'Graphoxide Graph',
        column,
        { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [this.extensionUri] },
      );
      this.panel.iconPath = vscode.Uri.joinPath(this.extensionUri, 'media', 'activity.svg');
      this.panel.onDidDispose(() => {
        this.messageSubscription?.dispose();
        this.messageSubscription = undefined;
        this.panel = undefined;
      });
      this.messageSubscription = this.panel.webview.onDidReceiveMessage((message: unknown) => this.handleMessage(message));
    }
    this.panel.title = selectedCommunity ? `Graphoxide · Community ${selectedCommunity}` : 'Graphoxide Graph';
    this.panel.webview.html = renderHtml(this.panel.webview, model, selectedCommunity);
    this.panel.reveal(column, false);
  }

  refresh(model: GraphModel): void {
    this.model = model;
    if (this.panel) this.panel.webview.html = renderHtml(this.panel.webview, model, this.selectedCommunity);
  }

  dispose(): void {
    this.messageSubscription?.dispose();
    this.panel?.dispose();
  }

  private handleMessage(message: unknown): void {
    if (typeof message !== 'object' || message === null || !('type' in message)) return;
    const value = message as { type: unknown; id?: unknown };
    if (typeof value.id !== 'string' || !this.model?.getNode(value.id)) return;
    if (value.type === 'reveal') this.onReveal(value.id);
    if (value.type === 'explain') this.onExplain(value.id);
  }
}

function renderHtml(webview: vscode.Webview, model: GraphModel, selectedCommunity?: string): string {
  const config = vscode.workspace.getConfiguration('graphoxide');
  const maxNodes = config.get<number>('visualization.maxNodes', 750);
  let candidates = selectedCommunity
    ? model.snapshot.nodes.filter((node) => (node.community ?? 'unassigned') === selectedCommunity)
    : [...model.snapshot.nodes];
  const totalCandidates = candidates.length;
  if (candidates.length > maxNodes) {
    candidates = candidates.sort((a, b) => model.degree(b.id) - model.degree(a.id)).slice(0, maxNodes);
  }
  const included = new Set(candidates.map((node) => node.id));
  const nodes: VisualNode[] = candidates.map((node) => ({
    id: node.id,
    label: node.label,
    file: node.sourceFile,
    location: node.sourceLocation ?? '',
    kind: node.fileType,
    community: node.community ?? 'unassigned',
    communityName: node.communityName ?? (node.community === undefined ? 'Unassigned' : `Community ${node.community}`),
    degree: model.degree(node.id),
  }));
  const edges: VisualEdge[] = model.snapshot.edges
    .filter((edge) => included.has(edge.source) && included.has(edge.target))
    .map((edge: GraphEdge) => ({
      source: edge.source,
      target: edge.target,
      relation: edge.relation,
      confidence: edge.confidence ?? '',
    }));
  const data = JSON.stringify({ nodes, edges, totalCandidates, maxNodes }).replace(/</gu, '\\u003c').replace(/\u2028/gu, '\\u2028').replace(/\u2029/gu, '\\u2029');
  const nonce = randomBytes(18).toString('base64');
  const csp = `default-src 'none'; img-src ${webview.cspSource} data:; style-src 'nonce-${nonce}'; script-src 'nonce-${nonce}'`;

  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <meta http-equiv="Content-Security-Policy" content="${csp}">
  <title>Graphoxide Graph</title>
  <style nonce="${nonce}">
    :root { color-scheme: light dark; }
    * { box-sizing: border-box; }
    body { margin: 0; width: 100vw; height: 100vh; overflow: hidden; color: var(--vscode-foreground); background: var(--vscode-editor-background); font-family: var(--vscode-font-family); }
    header { height: 46px; display: flex; align-items: center; gap: 8px; padding: 7px 10px; border-bottom: 1px solid var(--vscode-panel-border); background: var(--vscode-sideBar-background); }
    input, select, button { height: 30px; color: var(--vscode-input-foreground); background: var(--vscode-input-background); border: 1px solid var(--vscode-input-border, transparent); border-radius: 2px; padding: 0 8px; font: inherit; }
    input { min-width: 180px; flex: 1; max-width: 360px; }
    button { cursor: pointer; color: var(--vscode-button-foreground); background: var(--vscode-button-background); }
    button:hover { background: var(--vscode-button-hoverBackground); }
    button.secondary { color: var(--vscode-foreground); background: var(--vscode-button-secondaryBackground); }
    #stats { margin-left: auto; color: var(--vscode-descriptionForeground); white-space: nowrap; font-size: 12px; }
    main { position: relative; display: flex; width: 100%; height: calc(100vh - 46px); }
    canvas { display: block; flex: 1; min-width: 0; cursor: grab; }
    canvas.dragging { cursor: grabbing; }
    aside { width: 300px; padding: 16px; border-left: 1px solid var(--vscode-panel-border); background: var(--vscode-sideBar-background); overflow: auto; }
    aside.empty { display: none; }
    aside h2 { margin: 0 0 8px; font-size: 17px; overflow-wrap: anywhere; }
    aside dl { margin: 16px 0; }
    aside dt { margin-top: 10px; color: var(--vscode-descriptionForeground); font-size: 11px; text-transform: uppercase; }
    aside dd { margin: 3px 0; overflow-wrap: anywhere; }
    aside .actions { display: flex; gap: 8px; }
    #matches { position: absolute; top: 6px; left: 10px; z-index: 2; width: 350px; max-height: 260px; overflow: auto; margin: 0; padding: 4px; list-style: none; color: var(--vscode-quickInput-foreground); background: var(--vscode-quickInput-background); border: 1px solid var(--vscode-widget-border); box-shadow: 0 4px 14px var(--vscode-widget-shadow); }
    #matches:empty { display: none; }
    #matches button { display: block; width: 100%; border: 0; text-align: left; color: inherit; background: transparent; }
    #matches button:hover { background: var(--vscode-list-hoverBackground); }
    .legend { position: absolute; bottom: 8px; left: 10px; padding: 5px 8px; color: var(--vscode-descriptionForeground); background: color-mix(in srgb, var(--vscode-editor-background) 88%, transparent); font-size: 11px; pointer-events: none; }
    @media (max-width: 700px) { aside { position: absolute; right: 0; height: 100%; width: min(300px, 70vw); } #stats { display: none; } select { max-width: 110px; } }
  </style>
</head>
<body>
  <header>
    <input id="search" type="search" placeholder="Find node…" aria-label="Find node">
    <select id="community" aria-label="Filter community"><option value="">All communities</option></select>
    <select id="relation" aria-label="Filter relation"><option value="">All relations</option></select>
    <button id="reset" class="secondary" title="Reset view">Reset</button>
    <span id="stats"></span>
  </header>
  <main>
    <canvas id="graph" role="img" aria-label="Interactive code knowledge graph"></canvas>
    <ul id="matches"></ul>
    <div class="legend">Scroll to zoom · drag to pan · double-click a node to open source</div>
    <aside id="details" class="empty" aria-live="polite"></aside>
  </main>
  <script nonce="${nonce}">
    'use strict';
    const vscode = acquireVsCodeApi();
    const data = ${data};
    const canvas = document.getElementById('graph');
    const ctx = canvas.getContext('2d');
    const details = document.getElementById('details');
    const search = document.getElementById('search');
    const matches = document.getElementById('matches');
    const communitySelect = document.getElementById('community');
    const relationSelect = document.getElementById('relation');
    const stats = document.getElementById('stats');
    const colors = ['#4f8ff7','#f0648b','#46b39a','#ffb84d','#9b72cf','#55a7c8','#dc7557','#8fa83c','#c76da7','#6c8be7','#d09547','#46a7a0'];
    const nodeById = new Map(data.nodes.map(node => [node.id, node]));
    const positions = new Map();
    let selected = null;
    let scale = 1;
    let offsetX = 0;
    let offsetY = 0;
    let dragging = false;
    let dragX = 0;
    let dragY = 0;
    let communityFilter = '';
    let relationFilter = '';

    const escapeHtml = value => String(value).replace(/[&<>"']/g, char => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[char]));
    const communities = [...new Map(data.nodes.map(node => [node.community, node.communityName])).entries()].sort((a,b) => a[1].localeCompare(b[1]));
    for (const [id,name] of communities) { const option=document.createElement('option'); option.value=id; option.textContent=name; communitySelect.append(option); }
    const relations = [...new Set(data.edges.map(edge => edge.relation))].sort();
    for (const relation of relations) { const option=document.createElement('option'); option.value=relation; option.textContent=relation; relationSelect.append(option); }

    function visibleEdges() { return data.edges.filter(edge => !relationFilter || edge.relation === relationFilter); }
    function visibleNodes() {
      if (!communityFilter && !relationFilter) return data.nodes;
      const edgeIds = relationFilter ? new Set(visibleEdges().flatMap(edge => [edge.source,edge.target])) : null;
      return data.nodes.filter(node => (!communityFilter || node.community === communityFilter) && (!edgeIds || edgeIds.has(node.id)));
    }
    function layout() {
      const groups = new Map();
      for (const node of data.nodes) { const group=groups.get(node.community)||[]; group.push(node); groups.set(node.community,group); }
      const entries=[...groups.entries()].sort((a,b)=>b[1].length-a[1].length);
      const columns=Math.max(1,Math.ceil(Math.sqrt(entries.length)));
      const cellW=340, cellH=300;
      entries.forEach(([id,nodes],groupIndex) => {
        const cx=(groupIndex%columns)*cellW+cellW/2;
        const cy=Math.floor(groupIndex/columns)*cellH+cellH/2;
        const rings=Math.ceil(Math.sqrt(nodes.length/5));
        nodes.sort((a,b)=>b.degree-a.degree).forEach((node,index)=>{
          if(index===0){positions.set(node.id,{x:cx,y:cy});return;}
          const ring=Math.max(1,Math.ceil(Math.sqrt(index/5)));
          const previous=ring===1?1:5*(ring-1)*(ring-1);
          const within=index-previous;
          const count=Math.max(6,5*ring*2);
          const angle=(within/count)*Math.PI*2+(Number.parseInt(id,10)||groupIndex)*0.33;
          const radius=45+ring*34;
          positions.set(node.id,{x:cx+Math.cos(angle)*radius,y:cy+Math.sin(angle)*radius});
        });
      });
      fit();
    }
    function fit() {
      const values=[...positions.values()]; if(!values.length)return;
      const minX=Math.min(...values.map(p=>p.x))-60,maxX=Math.max(...values.map(p=>p.x))+60;
      const minY=Math.min(...values.map(p=>p.y))-60,maxY=Math.max(...values.map(p=>p.y))+60;
      scale=Math.min((canvas.clientWidth||1)/(maxX-minX),(canvas.clientHeight||1)/(maxY-minY),1.4)*0.9;
      offsetX=(canvas.clientWidth-(minX+maxX)*scale)/2; offsetY=(canvas.clientHeight-(minY+maxY)*scale)/2;
      draw();
    }
    function theme(name,fallback) { return getComputedStyle(document.body).getPropertyValue(name).trim()||fallback; }
    function resize(){const ratio=window.devicePixelRatio||1;canvas.width=Math.floor(canvas.clientWidth*ratio);canvas.height=Math.floor(canvas.clientHeight*ratio);ctx.setTransform(ratio,0,0,ratio,0,0);draw();}
    function screen(position){return{x:position.x*scale+offsetX,y:position.y*scale+offsetY};}
    function draw(){
      const width=canvas.clientWidth,height=canvas.clientHeight;ctx.clearRect(0,0,width,height);
      const nodes=visibleNodes(),ids=new Set(nodes.map(node=>node.id)),edges=visibleEdges().filter(edge=>ids.has(edge.source)&&ids.has(edge.target));
      ctx.lineWidth=Math.max(.4,Math.min(1.2,scale));ctx.strokeStyle=theme('--vscode-editorWidget-border','#777');ctx.globalAlpha=.38;
      for(const edge of edges){const a=positions.get(edge.source),b=positions.get(edge.target);if(!a||!b)continue;const sa=screen(a),sb=screen(b);ctx.beginPath();ctx.moveTo(sa.x,sa.y);ctx.lineTo(sb.x,sb.y);ctx.stroke();}
      ctx.globalAlpha=1;
      const communityIds=new Map(communities.map(([id],index)=>[id,index]));
      for(const node of nodes){const pos=positions.get(node.id);if(!pos)continue;const p=screen(pos);if(p.x<-30||p.y<-30||p.x>width+30||p.y>height+30)continue;const radius=Math.max(3.5,Math.min(11,(4+Math.sqrt(node.degree))*Math.sqrt(scale)));ctx.beginPath();ctx.arc(p.x,p.y,radius,0,Math.PI*2);ctx.fillStyle=colors[(communityIds.get(node.community)||0)%colors.length];ctx.fill();if(selected===node.id){ctx.lineWidth=3;ctx.strokeStyle=theme('--vscode-focusBorder','#fff');ctx.stroke();}if(scale>.75&&node.degree>2){ctx.fillStyle=theme('--vscode-editor-foreground','#ddd');ctx.font='11px '+theme('--vscode-font-family','sans-serif');ctx.fillText(node.label,p.x+radius+3,p.y+4);}}
      stats.textContent=nodes.length+' nodes · '+edges.length+' edges'+(data.totalCandidates>data.maxNodes?' · top '+data.maxNodes+' of '+data.totalCandidates:'');
    }
    function hit(clientX,clientY){const rect=canvas.getBoundingClientRect(),x=clientX-rect.left,y=clientY-rect.top;let best=null,bestDistance=18;for(const node of visibleNodes()){const pos=positions.get(node.id);if(!pos)continue;const p=screen(pos),distance=Math.hypot(p.x-x,p.y-y);if(distance<bestDistance){best=node;bestDistance=distance;}}return best;}
    function selectNode(node,center=false){selected=node?.id||null;matches.replaceChildren();if(!node){details.classList.add('empty');draw();return;}details.classList.remove('empty');details.innerHTML='<h2>'+escapeHtml(node.label)+'</h2><div>'+escapeHtml(node.kind)+'</div><dl><dt>Identifier</dt><dd><code>'+escapeHtml(node.id)+'</code></dd><dt>Source</dt><dd>'+escapeHtml(node.file||'No source file')+(node.location?':'+escapeHtml(node.location):'')+'</dd><dt>Community</dt><dd>'+escapeHtml(node.communityName)+'</dd><dt>Connections</dt><dd>'+node.degree+'</dd></dl><div class="actions"><button id="reveal">Open source</button><button id="explain" class="secondary">Explain</button></div>';document.getElementById('reveal').onclick=()=>vscode.postMessage({type:'reveal',id:node.id});document.getElementById('explain').onclick=()=>vscode.postMessage({type:'explain',id:node.id});if(center){const p=positions.get(node.id);if(p){offsetX=canvas.clientWidth/2-p.x*scale;offsetY=canvas.clientHeight/2-p.y*scale;}}draw();}
    search.addEventListener('input',()=>{const term=search.value.trim().toLowerCase();matches.replaceChildren();if(!term)return;data.nodes.filter(node=>node.label.toLowerCase().includes(term)||node.id.toLowerCase().includes(term)||node.file.toLowerCase().includes(term)).sort((a,b)=>b.degree-a.degree).slice(0,20).forEach(node=>{const item=document.createElement('li'),button=document.createElement('button');button.textContent=node.label+' — '+(node.file||node.kind);button.onclick=()=>{search.value=node.label;selectNode(node,true);};item.append(button);matches.append(item);});});
    communitySelect.addEventListener('change',()=>{communityFilter=communitySelect.value;selected=null;draw();});
    relationSelect.addEventListener('change',()=>{relationFilter=relationSelect.value;selected=null;draw();});
    document.getElementById('reset').addEventListener('click',()=>{communityFilter='';relationFilter='';communitySelect.value='';relationSelect.value='';search.value='';matches.replaceChildren();selected=null;details.classList.add('empty');fit();});
    canvas.addEventListener('pointerdown',event=>{dragging=true;dragX=event.clientX;dragY=event.clientY;canvas.setPointerCapture(event.pointerId);canvas.classList.add('dragging');});
    canvas.addEventListener('pointermove',event=>{if(!dragging)return;offsetX+=event.clientX-dragX;offsetY+=event.clientY-dragY;dragX=event.clientX;dragY=event.clientY;draw();});
    canvas.addEventListener('pointerup',event=>{dragging=false;canvas.releasePointerCapture(event.pointerId);canvas.classList.remove('dragging');const node=hit(event.clientX,event.clientY);if(node)selectNode(node);});
    canvas.addEventListener('dblclick',event=>{const node=hit(event.clientX,event.clientY);if(node)vscode.postMessage({type:'reveal',id:node.id});});
    canvas.addEventListener('wheel',event=>{event.preventDefault();const rect=canvas.getBoundingClientRect(),x=event.clientX-rect.left,y=event.clientY-rect.top;const worldX=(x-offsetX)/scale,worldY=(y-offsetY)/scale;scale=Math.max(.12,Math.min(5,scale*Math.exp(-event.deltaY*.001)));offsetX=x-worldX*scale;offsetY=y-worldY*scale;draw();},{passive:false});
    window.addEventListener('resize',resize);window.addEventListener('keydown',event=>{if(event.key==='Escape')selectNode(null);});
    layout();resize();
  </script>
</body>
</html>`;
}
