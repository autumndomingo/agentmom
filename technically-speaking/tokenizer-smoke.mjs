import assert from "node:assert/strict";
import { teachingTokenCount, teachingTokens } from "./tokenizer.js";

const words = teachingTokens("hello world");
assert.equal(teachingTokenCount("hello world"), 2);
assert.deepEqual(
  words.map((piece) => piece.text),
  ["hello", " world"],
);

const emoji = "👋🏽 hello";
const emojiPieces = teachingTokens(emoji);
assert.equal(emojiPieces.map((piece) => piece.text).join(""), emoji);
assert.equal(
  emojiPieces.reduce((total, piece) => total + piece.ids.length, 0),
  teachingTokenCount(emoji),
);

console.log("Tokenizer smoke test passed.");
