const fs = require('fs');
const path = require('path');
const browserify = require('browserify');
const terser = require('terser');
const packages = ['moment','crypto-js','cheerio','xml2js','ajv','chai'];
(async()=>{
  for (const name of packages) {
    const source=await new Promise((resolve,reject)=>browserify({basedir:__dirname}).require(name,{expose:name}).bundle((error,buffer)=>error?reject(error):resolve(buffer.toString())));
    const output=await terser.minify(source,{compress:true,mangle:true,format:{comments:false}});
    fs.writeFileSync(path.join(__dirname,name+'.min.js'),output.code);
  }
  const notices=[];
  function visit(directory){
    for(const entry of fs.readdirSync(directory,{withFileTypes:true})){
      if(!entry.isDirectory())continue;
      const root=path.join(directory,entry.name);
      if(entry.name.startsWith('@')){visit(root);continue;}
      const manifest=path.join(root,'package.json');if(!fs.existsSync(manifest))continue;
      const pkg=JSON.parse(fs.readFileSync(manifest,'utf8'));
      for(const file of fs.readdirSync(root).filter(file=>/^(licen[sc]e|copying|notice)(\.|$)/i.test(file))){
        const candidate=path.join(root,file);if(fs.statSync(candidate).isFile())notices.push(`\n--- ${pkg.name}@${pkg.version}: ${file} ---\n`+fs.readFileSync(candidate,'utf8'));
      }
      if(fs.existsSync(path.join(root,'node_modules')))visit(path.join(root,'node_modules'));
    }
  }
  visit(path.join(__dirname,'node_modules'));
  fs.writeFileSync(path.join(__dirname,'THIRD_PARTY_NOTICES.txt'),notices.join('\n'));
})().catch(error=>{console.error(error);process.exitCode=1});
