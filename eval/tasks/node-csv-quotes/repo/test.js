'use strict';

const assert = require('node:assert/strict');
const { parseLine, parse } = require('./csv');

assert.deepEqual(parseLine('a,b,c'), ['a', 'b', 'c']);
assert.deepEqual(parseLine('a,,c,'), ['a', '', 'c', '']);
assert.deepEqual(parseLine('one'), ['one']);
assert.deepEqual(parse('x,y\n\n1,2\r\n3,4\n'), [
  ['x', 'y'],
  ['1', '2'],
  ['3', '4'],
]);

console.log('csv tests passed');
