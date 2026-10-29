import { createInterface } from 'node:readline';
import { listSessions, getSessionMessages } from '@anthropic-ai/claude-agent-sdk';
const input=createInterface({input:process.stdin,crlfDelay:Infinity});
input.once('line',async line=>{
 try {
  if(Buffer.byteLength(line)>65536)throw new Error('Request too large');
  const request=JSON.parse(line);
  if(typeof request.workspace!=='string')throw new Error('Invalid metadata operation');
  if(request.operation==='messages'){
   if(typeof request.session_id!=='string'||!Number.isInteger(request.offset)||request.offset<0||request.offset>1000000)throw new Error('Invalid transcript cursor');
   const messages=await getSessionMessages(request.session_id,{dir:request.workspace,limit:100,offset:request.offset,includeSystemMessages:false});
   process.stdout.write(JSON.stringify({ok:true,messages})+'\n',()=>process.exit(0));return;
  }
  if(request.operation!=='list')throw new Error('Invalid metadata operation');
  const rows=await listSessions({dir:request.workspace,limit:100,offset:0,includeWorktrees:false});
  const sessions=rows.map(row=>({id:row.sessionId,title:row.customTitle||row.summary||row.firstPrompt||'Imported session',cwd:row.cwd||request.workspace,updated_at:row.lastModified}));
  process.stdout.write(JSON.stringify({ok:true,sessions})+'\n',()=>process.exit(0));
 }catch{process.stdout.write(JSON.stringify({ok:false,error:'Local Claude history inspection failed'})+'\n',()=>process.exit(1));}
});
