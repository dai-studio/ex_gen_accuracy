import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { parseXml, XmlError } from '../src/xml.ts';

describe('parseXml', () => {
  it('parses elements, attributes, text and positions', () => {
    const root = parseXml(`<?xml version="1.0"?>
<!-- leading comment -->
<root a="1" b='two'>
  <child x="&lt;&amp;&gt;&quot;&apos;"/>
  <text>hello &#65;&#x42; <![CDATA[<raw & stuff>]]></text>
</root>`);
    assert.equal(root.name, 'root');
    assert.deepEqual({ ...root.attrs }, { a: '1', b: 'two' });
    assert.equal(root.line, 3);
    assert.equal(root.children.length, 2);
    assert.equal(root.children[0]!.attrs.x, `<&>"'`);
    assert.equal(root.children[0]!.line, 4);
    assert.equal(root.children[1]!.text, 'hello AB <raw & stuff>');
  });

  it('ignores comments and processing instructions inside elements', () => {
    const root = parseXml('<a><!-- c --><?pi x?><b/>text</a>');
    assert.equal(root.children.length, 1);
    assert.equal(root.text, 'text');
  });

  it('accepts a UTF-8 BOM and names with dashes, dots and colons', () => {
    const root = parseXml('﻿<rate-limit ns:x.y="1"/>');
    assert.equal(root.name, 'rate-limit');
    assert.equal(root.attrs['ns:x.y'], '1');
  });

  const bad: Array<[string, RegExp]> = [
    ['<a><b></a>', /Mismatched closing tag/],
    ['<a>', /Unclosed element/],
    ['<a x="1" x="2"/>', /Duplicate attribute/],
    ['<a x=1/>', /quoted attribute/],
    ['<a x="<"/>', /not allowed in attribute/],
    ['<a>&bogus;</a>', /Unknown entity/],
    ['<a>fish & chips</a>', /Unescaped "&"/],
    ['<!DOCTYPE a [<!ENTITY x "y">]><a/>', /DOCTYPE/],
    ['<a/><b/>', /after the root element/],
    ['', /root element/],
    ['<a b="1"c="2"/>', /whitespace before attribute/],
  ];
  for (const [src, pattern] of bad) {
    it(`rejects ${JSON.stringify(src)}`, () => {
      assert.throws(() => parseXml(src), (err: unknown) => err instanceof XmlError && pattern.test(err.message));
    });
  }

  it('reports line and column of errors', () => {
    try {
      parseXml('<a>\n  <b>\n</a>');
      assert.fail('should throw');
    } catch (err) {
      assert.ok(err instanceof XmlError);
      assert.equal(err.line, 3);
      assert.equal(err.column, 3);
    }
  });
});
