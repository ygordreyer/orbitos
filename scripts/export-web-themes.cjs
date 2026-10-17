#!/usr/bin/env node
// Export one native pack per theme with descriptor settings and resolved geometry.
const fs=require('fs'),path=require('path');
const source=path.resolve(process.argv[2]||'../miyoo-ui-sim');
const output=path.resolve(process.argv[3]||'themes');
const L=require(path.join(source,'dist/theme-layout.js'));
L.setSchema(JSON.parse(fs.readFileSync(path.join(source,'dist/themes/theme.schema.json'))));
const old=new Set('id kind rect asset_id binding role color radius max_lines overflow align fit layout visible_count item_extent gap padding art_size caption_height caption_lines frame_color focus_color focus_style focus_width glow_radius group max_items fade_edge clip_shape fade_width battery_style blur_radius mask_asset_id presentation logo logo_when_no_cover'.split(' '));
const typeKeys=['font','size','line_height','weight','letter_spacing','transform','color'];
const report=[],styleRefs=new Map();
function exportVariant(original,shape,selected={}){
 const id=original.id;original=L.settingsDescriptor(original,{...selected,cover_shape:shape});
 L.seal(original);
  const pack=JSON.parse(JSON.stringify(original));
  pack.id=id;pack.name=original.name+(shape==='tall'?' Tall':'');
  pack.revision='native-ink-1';pack.motion='none';pack.alignment={...pack.alignment,cover_shape:shape};
  for(const [screen,spec] of Object.entries(pack.screens)){
   spec.components=L.resolve(original,screen,shape).map(c=>{
    const out={},paint={};
    for(const [key,value] of Object.entries(c)){
     if(key.startsWith('_')||key==='shapes'||typeKeys.includes(key)&&key!=='color'||key==='focus_weight'||key==='items')continue;
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
    if(c.kind==='hints'&&c.items)paint.hint_items=c.items;
    if(c.kind==='keyboard')paint.keyboard_layout='qwerty';
    out.paint=paint;return out;
   });
   if(spec.components.some(c=>c.binding==='selected_cover')){
    for(const c of spec.components)if(c.logo||c.binding==='selected_logo')c.logo_when_no_cover=true;
   }
  }
  pack.ink_typography=Object.fromEntries(Object.entries(original.tokens.typography).map(([k,t])=>[k,Object.fromEntries(typeKeys.filter(key=>t[key]!==undefined).map(key=>[key,t[key]]))]));
  pack.tokens.typography=Object.fromEntries(Object.entries(pack.tokens.typography).map(([k,t])=>[k,{font:t.font,size:t.size,line_height:t.line_height}]));
  const counts=new Map();for(const s of Object.values(pack.screens))for(const c of s.components){const k=JSON.stringify(c.paint);counts.set(k,(counts.get(k)||0)+1);}
  pack.paint_presets={};const refs=new Map();for(const [k,n] of counts)if(k.length>20){if(!styleRefs.has(k))styleRefs.set(k,'p'+styleRefs.size);const id=styleRefs.get(k);refs.set(k,id);pack.paint_presets[id]=JSON.parse(k);}
  for(const s of Object.values(pack.screens))for(const c of s.components){const ref=refs.get(JSON.stringify(c.paint));if(ref){delete c.paint;c.paint_ref=ref;}}
  return pack;
}
function difference(base,next){
 if(JSON.stringify(base)===JSON.stringify(next))return {};
 const patch={};for(const key of new Set([...Object.keys(base),...Object.keys(next)])){
  if(!Object.hasOwn(next,key))patch[key]=null;
  else if(JSON.stringify(base[key])!==JSON.stringify(next[key]))patch[key]=base[key]&&next[key]&&typeof base[key]==='object'&&typeof next[key]==='object'&&!Array.isArray(base[key])&&!Array.isArray(next[key])?difference(base[key],next[key]):next[key];
 }
 return patch;
}
for(const id of ['orbit-hero','orbit-row','big-picture','compact-plus']){
 const original=JSON.parse(fs.readFileSync(path.join(source,'dist/themes',id,'theme.json')));L.seal(original);
 const defaults=L.selectedSettings(original),pack=exportVariant(original,defaults.cover_shape||'square',defaults);
 pack.settings=(original.settings||[]).map(setting=>({id:setting.id,label:setting.label,default:setting.default,options:setting.options.map(option=>({id:option.id,label:option.label,patch:option.id===setting.default?{}:difference(pack,exportVariant(original,setting.id==='cover_shape'?option.id:defaults.cover_shape,{...defaults,[setting.id]:option.id}))}))}));
 // Color options replace all nine keys, even when one happens to match the base.
 for(const setting of pack.settings)if(setting.id==='color_scheme')for(const option of setting.options)if(option.id!==setting.default)option.patch.tokens={...option.patch.tokens,colors:L.settingsDescriptor(original,{...defaults,color_scheme:option.id}).tokens.colors};
 const dir=path.join(output,pack.id);fs.mkdirSync(dir,{recursive:true});
 const bytes=Buffer.from(JSON.stringify(pack));if(bytes.length>(pack.settings.length?131072:65536))throw Error(pack.id+' exceeds manifest limit: '+bytes.length);
 fs.writeFileSync(path.join(dir,'theme.json'),bytes);
 const assets=new Map(Object.values(pack.assets).map(asset=>[asset.path,asset]));
 for(const setting of pack.settings)for(const option of setting.options)for(const asset of Object.values(option.patch.assets||{}))if(asset)assets.set(asset.path,asset);
 for(const asset of assets.values()){const to=path.join(dir,asset.path);fs.mkdirSync(path.dirname(to),{recursive:true});fs.copyFileSync(path.join(source,'dist/themes',id,asset.path),to)}
 report.push({id:pack.id,name:pack.name,settings:pack.settings.map(({id})=>id),manifest_bytes:bytes.length});
}
fs.writeFileSync(path.join(output,'choices.json'),JSON.stringify(report,null,2)+'\n');
console.log(report.map(p=>`${p.id}: ${p.name}, ${p.manifest_bytes} bytes`).join('\n'));
