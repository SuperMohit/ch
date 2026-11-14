'use strict';

// Hidden check for the node-csv-quotes task. A .cjs file is CommonJS whatever the
// workspace's package.json says, and `node:` modules are always Node's own.

const assert = require('node:assert/strict');
const { parseLine, parse } = require('./csv');

// Unquoted fields work as before.
assert.deepEqual(parseLine('a,b,c'), ['a', 'b', 'c']);
assert.deepEqual(parseLine('a,,c,'), ['a', '', 'c', '']);
assert.deepEqual(parseLine('one'), ['one']);

// Quoted fields.
assert.deepEqual(parseLine('"a,b",c'), ['a,b', 'c']);
assert.deepEqual(parseLine('x,"hello, world"'), ['x', 'hello, world']);
assert.deepEqual(parseLine('1,"2,3",4'), ['1', '2,3', '4']);
assert.deepEqual(parseLine('"a","b"'), ['a', 'b']);
assert.deepEqual(parseLine('"say ""hi""",2'), ['say "hi"', '2']);
assert.deepEqual(parseLine('"",1'), ['', '1']);
assert.deepEqual(parseLine('""""'), ['"']);
assert.deepEqual(parseLine('a,""'), ['a', '']);
assert.deepEqual(parseLine('"a,,b",,"c"'), ['a,,b', '', 'c']);

// And through parse.
assert.deepEqual(parse('name,quote\nada,"x, y"\n\nbob,"say ""no"""\n'), [
  ['name', 'quote'],
  ['ada', 'x, y'],
  ['bob', 'say "no"'],
]);

console.log('hidden csv tests passed');
