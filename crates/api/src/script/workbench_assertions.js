// Pinned Ajv 6 implements draft-07, including local references and standard
// formats. External schemas must be supplied in the document; no network loader
// or coercion is installed. Unknown keywords and formats are errors.
function workbenchValidateSchema(value, schema, options) {
  if (options !== undefined && (options === null || typeof options !== 'object' || Object.keys(options).length)) throw new Error('JSON Schema options are unsupported in Workbench');
  let nodes=0;
  const bound=(value,depth)=>{
    if(++nodes>16384||depth>48)throw new Error('JSON Schema exceeds its size or nesting limit');
    if(value&&typeof value==='object')for(const key of Object.keys(value))bound(value[key],depth+1);
  };
  bound(schema,0);bound(value,0);
  const references=node=>{
    if(!node||typeof node!=='object')return;
    if(typeof node.$ref==='string'){
      const ref=node.$ref;
      if(!ref.startsWith('#'))throw new Error("can't resolve reference "+ref+': external schema loading is unavailable');
      if(ref.startsWith('#/')){
        let target=schema;
        for(const token of decodeURIComponent(ref.slice(2)).split('/').map(part=>part.replace(/~1/g,'/').replace(/~0/g,'~'))){
          if(!target||typeof target!=='object'||!Object.prototype.hasOwnProperty.call(target,token))throw new Error("can't resolve reference "+ref);
          target=target[token];
        }
      }
    }
    for(const key of ['properties','patternProperties','definitions'])if(node[key]&&typeof node[key]==='object')for(const name of Object.keys(node[key]))references(node[key][name]);
    for(const key of ['additionalProperties','additionalItems','contains','propertyNames','not','if','then','else'])references(node[key]);
    for(const key of ['allOf','anyOf','oneOf'])if(Array.isArray(node[key]))node[key].forEach(references);
    if(Array.isArray(node.items))node.items.forEach(references);else references(node.items);
    if(node.dependencies&&typeof node.dependencies==='object')for(const name of Object.keys(node.dependencies))if(!Array.isArray(node.dependencies[name]))references(node.dependencies[name]);
  };
  references(schema);
  const Ajv=require('ajv');
  const validator=new Ajv({allErrors:false,strictKeywords:true,unknownFormats:false,logger:false,schemaId:'auto'});
  const check=validator.compile(schema);
  if(!check(value))throw new Error('JSON Schema validation failed: '+validator.errorsText(check.errors));
}

function workbenchResponseAssertions(response) {
  const methods = {
    status: expected => {
      if ((typeof expected === 'string' ? response.status : response.code) !== expected) throw new Error('Expected response status ' + expected + ', got ' + response.code);
    },
    header: function(name, expected) {
      if (!response.headers.has(name)) throw new Error('Missing response header ' + name);
      if (arguments.length > 1 && response.headers.get(name) !== expected) throw new Error('Unexpected response header value for ' + name);
    },
    body: function(expected) {
      if (arguments.length ? response.text() !== expected : response.text().length === 0) throw new Error('Unexpected response body');
    },
    jsonSchema: (schema, options) => workbenchValidateSchema(response.json(), schema, options),
  };
  const chain = new Proxy(Object.create(null), {get(_, key) {
    if (key === 'to' || key === 'have' || key === 'be') return chain;
    if (Object.prototype.hasOwnProperty.call(methods, key)) return methods[key];
    throw new Error('Unsupported Workbench response assertion: ' + String(key));
  }});
  return chain;
}
