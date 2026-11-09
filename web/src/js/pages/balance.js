// The balance page, balance.html: sends the token the operator gave to the pool, in a request header and never
// in a URL, and shows what its address is owed in each pool. The page keeps none of the token.

import * as api from "../api.js";
import { bind } from "../bind.js";
import "../common.js";

const form = document.getElementById("balance-form");
const input = form.elements.token;
const button = form.querySelector("button");
const answer = document.getElementById("balance-answer");
const result = document.getElementById("balance-result");

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  button.disabled = true;
  bind(answer, {});
  try {
    const [{ pools }, name] = await Promise.all([api.balance(input.value.trim()), api.poolNames()]);
    input.value = "";
    bind(result, { pools }, { pool: name });
    result.hidden = false;
  } catch (e) {
    result.hidden = true;
    bind(answer, e.code === "unauthorized" ? { unauthorized: true } : { unreachable: true });
  }
  button.disabled = false;
});
