import { Tiktoken } from "js-tiktoken/lite";
import o200kBase from "js-tiktoken/ranks/o200k_base";

const tokenizer = new Tiktoken(o200kBase);

export function teachingTokenCount(text) {
  return tokenizer.encode(text).length;
}

export function teachingTokens(text) {
  const pieces = [];
  let pendingIds = [];

  for (const id of tokenizer.encode(text)) {
    pendingIds.push(id);
    const decoded = tokenizer.decode(pendingIds);
    if (!decoded.includes("�")) {
      pieces.push({ ids: pendingIds, text: decoded });
      pendingIds = [];
    }
  }

  if (pendingIds.length) {
    pieces.push({ ids: pendingIds, text: tokenizer.decode(pendingIds) });
  }

  return pieces;
}
