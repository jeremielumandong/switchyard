
'use strict';
for (const name of ['fetch','WebSocket','XMLHttpRequest']) {
  Object.defineProperty(globalThis,name,{configurable:false,get(){throw new Error(name+' is unavailable in the Workbench sandbox');}});
}
const owns=(values,k)=>Object.prototype.hasOwnProperty.call(values,String(k));
const read=(values,k)=>owns(values,k)?values[String(k)]:undefined;
const runtimeNow=Date.now(),runtimeValues=Object.create(null),runtimeResolve=globalThis.__aoRuntimeVariable;
delete globalThis.__aoRuntimeVariable;
const runtime=k=>{k=String(k);if(!owns(runtimeValues,k))runtimeValues[k]=runtimeResolve(k,runtimeNow);return runtimeValues[k];};
const lookup=(k,stack=[])=>{k=String(k);const value=read(__ao.localVariables,k)??read(__ao.iterationData,k)??read(__ao.environment,k)??read(__ao.collectionVariables,k)??read(__ao.globals,k);if(value===undefined)return runtime(k);if(stack.includes(k)||stack.length>=32)throw new Error('Variable cycle or nesting limit: '+stack.concat(k).join(' -> '));return replaceIn(value,stack.concat(k));};
const replaceIn=(s,stack=[])=>String(s).replace(/{{\s*([^{}]+?)\s*}}/g,(_,k)=>{if(k.startsWith('vault.')){const name=k.slice(6);if(!owns(__ao.vault,name))throw new Error("Vault secret '"+name+"' is missing or unavailable");return __ao.vault[name];}const value=lookup(k,stack);if(value===undefined&&k.startsWith('$'))throw new Error('Unsupported Workbench dynamic variable: '+k);return value??'';});
const scope=name=>({get:k=>read(__ao[name],k),has:k=>owns(__ao[name],k),set:(k,v)=>{Object.defineProperty(__ao[name],String(k),{value:String(v),writable:true,configurable:true,enumerable:true})},unset:k=>{delete __ao[name][String(k)]},clear:()=>{__ao[name]={}},toObject:()=>Object.assign({},__ao[name]),replaceIn});
const globals=scope('globals');
const vars=scope('localVariables'),environment=scope('environment'),collectionVariables=scope('collectionVariables'),cookies=scope('cookies');
Object.defineProperty(environment,'name',{value:__ao.environmentName??undefined,enumerable:true});
vars.get=lookup;vars.has=k=>lookup(k)!==undefined;vars.toObject=()=>Object.assign({},__ao.globals,__ao.collectionVariables,__ao.environment,__ao.iterationData,__ao.localVariables);
const readOnlyScope=name=>Object.freeze({get:k=>__ao[name][String(k)],has:k=>Object.prototype.hasOwnProperty.call(__ao[name],String(k)),toObject:()=>Object.assign({},__ao[name]),replaceIn:s=>String(s).replace(/{{\s*([^{}]+?)\s*}}/g,(_,k)=>__ao[name][String(k)]??'')});
const iterationData=readOnlyScope('iterationData');
const encoding=Object.freeze({base64Encode:globalThis.__aoBase64Encode,base64Decode:globalThis.__aoBase64Decode});
delete globalThis.__aoBase64Encode;delete globalThis.__aoBase64Decode;
const headerApi=list=>({get:n=>{n=String(n).toLowerCase();const h=list.find(x=>String(x[0]).toLowerCase()===n);return h&&String(h[1])},has:n=>list.some(x=>String(x[0]).toLowerCase()===String(n).toLowerCase()),add:h=>list.push([String(h.key??h.name),String(h.value)]),upsert:h=>{const n=String(h.key??h.name).toLowerCase();const i=list.findIndex(x=>String(x[0]).toLowerCase()===n);const v=[String(h.key??h.name),String(h.value)];i<0?list.push(v):list[i]=v},remove:n=>{n=String(n).toLowerCase();for(let i=list.length-1;i>=0;i--)if(String(list[i][0]).toLowerCase()===n)list.splice(i,1)}});
__ao.request.headers=__ao.request.headers||[];__ao.requestHeaders=__ao.request.headers;
const req=__ao.request;req.headers=headerApi(__ao.requestHeaders);
let bodyText=String(req.body??'');
const requestBody={mode:'raw',get raw(){return bodyText},set raw(value){bodyText=String(value)},update(value){
  if(typeof value==='string')bodyText=value;
  else if(value&&typeof value==='object'&&(value.mode===undefined||value.mode==='raw')&&typeof value.raw==='string')bodyText=value.raw;
  else throw new Error('pm.request.body.update supports a string or a raw body object');
},toString(){return bodyText},toJSON(){return bodyText}};
Object.defineProperty(req,'body',{enumerable:true,get:()=>requestBody,set:value=>requestBody.update(value)});
let res=__ao.response;
if(res){res.headers=headerApi(res.headers||[]);res.text=()=>String(res.body??'');res.json=()=>JSON.parse(res.text());Object.defineProperty(res,'responseTime',{get:()=>res.responseTimeMs??0});res.to=workbenchResponseAssertions(res);}
function fail(message){throw new Error(message)}
let chaiModule;
const getChai=()=>{
  if(!chaiModule){chaiModule=require('chai');chaiModule.Assertion.addMethod('status',function(expected){const actual=this._obj;this.assert(actual&&actual.code===expected,'expected response status #{exp}, got #{act}','expected response status to differ from #{exp}',expected,actual&&actual.code)});}
  return chaiModule;
};
const expect=(...args)=>getChai().expect(...args);
expect.fail=(...args)=>getChai().expect.fail(...args);
const log=(level,args)=>{if(__ao.console.length>=256)throw new Error('Workbench console entry limit exceeded');__ao.console.push({level,message:args.map(v=>typeof v==='string'?v:JSON.stringify(v)).join(' ')})};
const consoleApi={log:(...a)=>log('log',a),info:(...a)=>log('info',a),warn:(...a)=>log('warn',a),error:(...a)=>log('error',a)};
let subrequestIndex=0;
const subrequestHeaders=input=>{
  if(input==null)return [];
  if(typeof input==='string')return input.split(/\r?\n/).filter(line=>line.trim()).map(line=>{const colon=line.indexOf(':');if(colon<1)throw new Error('pm.sendRequest header must be Name: value');return [line.slice(0,colon).trim(),line.slice(colon+1).trim()];});
  if(Array.isArray(input))return input.map(h=>{if(!h||typeof h!=='object')throw new Error('pm.sendRequest headers must be key/value pairs');const key=h.key??h.name??h[0],value=h.value??h[1];if(key==null||value==null)throw new Error('pm.sendRequest headers must be key/value pairs');return [String(key),String(value)];});
  if(typeof input==='object')return Object.entries(input).map(([k,v])=>[String(k),String(v)]);
  throw new Error('pm.sendRequest headers must be a string, array, or object');
};
const subrequestBody=(body,headers)=>{
  if(body==null)return null;
  if(typeof body==='string')return body;
  if(typeof body==='object'&&(body.mode===undefined||body.mode==='raw')&&typeof body.raw==='string')return body.raw;
  const contentType=value=>{if(!headers.some(h=>h[0].toLowerCase()==='content-type'))headers.push(['Content-Type',value]);};
  const fields=mode=>{if(!Array.isArray(body[mode]))throw new Error('pm.sendRequest '+mode+' requires an array');return body[mode].filter(field=>!field.disabled).map(field=>{
    if(field.type==='file'||field.src!==undefined)throw new Error('pm.sendRequest file bodies are unavailable in the sandbox');
    if(field.key==null)throw new Error('pm.sendRequest form fields require a key');return [String(field.key),String(field.value??'')];
  });};
  if(body.mode==='urlencoded'){
    contentType('application/x-www-form-urlencoded');
    const encode=value=>encodeURIComponent(value).replace(/%20/g,'+').replace(/[!'()~]/g,c=>'%'+c.charCodeAt(0).toString(16).toUpperCase());
    return fields('urlencoded').map(([key,value])=>encode(key)+'='+encode(value)).join('&');
  }
  if(body.mode==='formdata'){
    const values=fields('formdata');let boundary='----AgentOpsWorkbenchBoundary';
    while(values.some(([key,value])=>key.includes(boundary)||value.includes(boundary)))boundary+='x';
    contentType('multipart/form-data; boundary='+boundary);
    return values.map(([key,value])=>'--'+boundary+'\r\nContent-Disposition: form-data; name="'+key.replace(/\r/g,'%0D').replace(/\n/g,'%0A').replace(/"/g,'%22')+'"\r\n\r\n'+value+'\r\n').join('')+'--'+boundary+'--\r\n';
  }
  throw new Error('pm.sendRequest supports only text, raw, urlencoded, and text formdata bodies; use a saved request for file bodies');
};
const assertionsOnly=__ao.assertionsOnly;
const sendRequest=(input,callback)=>{if(assertionsOnly)throw new Error('Network requests are disabled while rerunning response tests');const raw=typeof input==='string'?{url:input}:input||{};const headers=subrequestHeaders(raw.header??raw.headers);const pending={method:String(raw.method||'GET'),url:String(raw.url||''),headers,body:subrequestBody(raw.body,headers)};const index=subrequestIndex++;if(index>=__ao.subrequestResponses.length){__ao.pendingSubrequest='SWITCHYARD_API_SUBREQUEST:'+JSON.stringify(pending);throw new Error(__ao.pendingSubrequest)};const answer=__ao.subrequestResponses[index];if(answer.error){const error=new Error(answer.error);if(typeof callback==='function'){callback(error,null);return undefined}throw error}answer.headers=headerApi(answer.headers||[]);answer.text=()=>String(answer.body??'');answer.json=()=>JSON.parse(answer.text());answer.stream={toString:()=>answer.text()};Object.defineProperty(answer,'responseTime',{get:()=>answer.responseTimeMs??0});answer.to=workbenchResponseAssertions(answer);if(typeof callback==='function')callback(null,answer);return answer;};
// A bounded timer queue is drained by the isolated worker between Promise jobs.
const timers=new Map();let timerId=0,timerCount=0,activeTest=null;
const timerSleep=globalThis.__aoSleep;delete globalThis.__aoSleep;
globalThis.setTimeout=(callback,delay=0,...args)=>{
  if(typeof callback!=='function')throw new Error('setTimeout requires a function');
  delay=Number(delay);if(!Number.isFinite(delay)||delay<0||delay>1000)throw new Error('Workbench timer delay must be between 0 and 1000 ms');
  if(++timerCount>128)throw new Error('Workbench timer limit exceeded');
  const id=++timerId;timers.set(id,{callback,args,due:Date.now()+delay,test:activeTest});return id;
};
globalThis.clearTimeout=id=>timers.delete(id);
const testFailure=(test,error)=>{const message=String(error&&error.message||error);if(message.startsWith('SWITCHYARD_API_SUBREQUEST:'))throw error;test.passed=false;test.error=message;test.pending=false;};
globalThis.__aoDrainTimer=()=>{
  let next=null;for(const [id,timer] of timers)if(!next||timer.due<next[1].due)next=[id,timer];
  if(!next)return false;
  const [id,timer]=next;timers.delete(id);timerSleep(Math.max(0,timer.due-Date.now()));
  const previous=activeTest;activeTest=timer.test;
  try{timer.callback(...timer.args)}catch(error){if(timer.test)testFailure(timer.test,error);else throw error}finally{activeTest=previous}
  return true;
};
const test=(name,fn)=>{
  const result={name:String(name),passed:false,pending:true};__ao.tests.push(result);
  const previous=activeTest;activeTest=result;
  const done=error=>{if(!result.pending)return;if(error)testFailure(result,error);else{result.pending=false;result.passed=true}};
  try{if(typeof fn!=='function')throw new Error('pm.test requires a function');const value=fn(done);
    if(value instanceof Promise)value.then(()=>done(),error=>testFailure(result,error));
    else if(fn.length===0)done();
  }catch(error){testFailure(result,error)}finally{activeTest=previous}
  return pm;
};
test.skip=name=>{__ao.tests.push({name:String(name),passed:false,skipped:true});return pm};
test.index=()=>__ao.tests.length;
const setNextRequest=target=>{if(target!==null&&typeof target!=='string')throw new Error('setNextRequest requires a request name, ID, or null');__ao.nextRequest=target===null?{kind:'stop'}:{kind:'request',target}};
globalThis.postman=Object.freeze({setNextRequest});
globalThis.pm={variables:vars,globals,environment,collectionVariables,iterationData,cookies,encoding,request:req,response:res,console:consoleApi,test,expect,sendRequest,execution:Object.freeze({setNextRequest})};
globalThis.__aoFinish=()=>{for(const test of __ao.tests)if(test.pending)testFailure(test,new Error('Async test did not complete: call done() or resolve the returned Promise'));if(__ao.pendingSubrequest)throw new Error(__ao.pendingSubrequest)};
globalThis.console=consoleApi;
Object.freeze(pm);

const loadModule=globalThis.__aoLoadModule,randomUint32=globalThis.__aoRandomUint32,moduleCache=Object.create(null);
delete globalThis.__aoLoadModule;delete globalThis.__aoRandomUint32;
Object.defineProperty(globalThis,'require',{value:name=>{name=String(name);if(!owns(moduleCache,name))moduleCache[name]=loadModule(name);return moduleCache[name]},writable:false,configurable:false});
Object.defineProperty(globalThis,'crypto',{value:Object.freeze({getRandomValues:array=>{
  if(!ArrayBuffer.isView(array)||array instanceof DataView||array instanceof Float32Array||array instanceof Float64Array||array.byteLength>65536)throw new Error('getRandomValues requires an integer typed array of at most 65536 bytes');
  const bytes=new Uint8Array(array.buffer,array.byteOffset,array.byteLength);for(let i=0;i<bytes.length;i+=4){const value=randomUint32();for(let j=0;j<4&&i+j<bytes.length;j++)bytes[i+j]=(value>>>(j*8))&255;}return array;
}})});
for(const [name,module] of [['CryptoJS','crypto-js'],['cheerio','cheerio']])Object.defineProperty(globalThis,name,{get:()=>require(module)});
globalThis.xml2Json=text=>{let result,error;require('xml2js').parseString(String(text),{explicitArray:false,explicitRoot:true},(err,value)=>{error=err;result=value});if(error)throw error;return result};
