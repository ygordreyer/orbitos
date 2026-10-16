#!/usr/bin/env node
// Resolve web shape geometry once. Native manifests contain no alternative shapes.
const fs=require('fs'),path=require('path');
const source=path.resolve(process.argv[2]||'../miyoo-ui-sim');
const output=path.resolve(process.argv[3]||'themes');
const L=require(path.join(source,'dist/theme-layout.js'));
L.setSchema(JSON.parse(fs.readFileSync(path.join(source,'dist/themes/theme.schema.json'))));
const old=new Set('id kind rect asset_id binding role color radius max_lines overflow align fit layout visible_count item_extent gap padding art_size caption_height caption_lines frame_color focus_color focus_style focus_width glow_radius group max_items fade_edge clip_shape fade_width battery_style blur_radius mask_asset_id presentation'.split(' '));
const typeKeys=['font','size','line_height','weight','letter_spacing','transform','color'];
const report=[];
for(const id of ['orbit-hero','orbit-row','big-picture','compact-plus']){
 const original=JSON.parse(fs.readFileSync(path.join(source,'dist/themes',id,'theme.json')));
 L.seal(original);
 for(const shape of ['square','tall']){
  const pack=JSON.parse(JSON.stringify(original));
  pack.id=id+(shape==='tall'?'-tall':'');pack.name=original.name+(shape==='tall'?' Tall':'');
  pack.revision='native-ink-1';pack.motion='none';pack.alignment={...pack.alignment,cover_shape:shape};
  for(const [screen,spec] of Object.entries(pack.screens)){
   spec.components=L.resolve(original,screen,shape).map(c=>{
    const out={},paint={};
    for(const [key,value] of Object.entries(c)){
     if(key.startsWith('_')||key==='shapes'||typeKeys.includes(key)&&key!=='color'||key==='focus_weight'||key==='items'||key==='shadow')continue;
     if(old.has(key))out[key]=value;else paint[key]=value;
    }
    if(c.role){
     paint.run=Object.fromEntries(typeKeys.filter(k=>c[k]!==undefined&&c[k]!==original.tokens.typography[c.role][k]).map(k=>[k,c[k]]));
     if(Object.keys(paint.run).length===0)delete paint.run;
     if(c.focus_weight!==undefined&&c.focus_weight!==(c.weight??original.tokens.typography[c.role].weight))paint.focus_run={weight:c.focus_weight};
    }
    // The controller supplies localization, actions and navigation identities.
    if(c.kind==='overlay'){out.color='background';out.radius=c.radius||0;}
    if(c.kind==='navigation'&&c.items)paint.icons=c.items.map(item=>item.icon);
    out.paint=paint;return out;
   });
  }
  pack.ink_typography=Object.fromEntries(Object.entries(original.tokens.typography).map(([k,t])=>[k,Object.fromEntries(typeKeys.filter(key=>t[key]!==undefined).map(key=>[key,t[key]]))]));
  pack.tokens.typography=Object.fromEntries(Object.entries(pack.tokens.typography).map(([k,t])=>[k,{font:t.font,size:t.size,line_height:t.line_height}]));
  const counts=new Map();for(const s of Object.values(pack.screens))for(const c of s.components){const k=JSON.stringify(c.paint);counts.set(k,(counts.get(k)||0)+1);}
  pack.paint_presets={};const refs=new Map();for(const [k,n] of counts)if(n>1&&k.length>20){const id='p'+refs.size;refs.set(k,id);pack.paint_presets[id]=JSON.parse(k);}
  for(const s of Object.values(pack.screens))for(const c of s.components){const ref=refs.get(JSON.stringify(c.paint));if(ref){delete c.paint;c.paint_ref=ref;}}
  const dir=path.join(output,pack.id);fs.mkdirSync(dir,{recursive:true});
  const bytes=Buffer.from(JSON.stringify(pack));if(bytes.length>65536)throw Error(pack.id+' exceeds manifest limit: '+bytes.length);
  fs.writeFileSync(path.join(dir,'theme.json'),bytes);
  for(const a of Object.values(pack.assets)){const to=path.join(dir,a.path);fs.mkdirSync(path.dirname(to),{recursive:true});fs.copyFileSync(path.join(source,'dist/themes',id,a.path),to);}
  report.push({id:pack.id,name:pack.name,shape,manifest_bytes:bytes.length});
 }
}
fs.writeFileSync(path.join(output,'choices.json'),JSON.stringify(report,null,2)+'\n');
console.log(report.map(p=>`${p.id}: ${p.name}, ${p.manifest_bytes} bytes`).join('\n'));
