import {mkdirSync,copyFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {dirname,join} from 'node:path';
import {spawnSync} from 'node:child_process';
const root=join(dirname(fileURLToPath(import.meta.url)),'..');
mkdirSync(join(root,'dist'),{recursive:true});
for(const name of ['index.mjs','history.mjs']){
 const source=join(root,'src',name);
 const check=spawnSync(process.execPath,['--check',source],{stdio:'inherit'});
 if(check.status!==0)process.exit(check.status||1);
 copyFileSync(source,join(root,'dist',name));
}
