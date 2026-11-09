// The blocks table of a page, newest first, read from a blocks resource (API reference section 2.8) and read
// again when the chain tip moves: common.js fires "puntohunter:tip" on the document, since block rows change
// only when a block is found or the tip moves.

import * as api from "./api.js";
import { bind } from "./bind.js";

// Fills `card`, which holds the blocks table's markup, from the blocks at `path`, such as
// "/fleets/<handle>/blocks". It is filled with `blocks`, and `none` when there are none; or with `failed` when
// the first read fails. A table already filled stays as it is until the next refresh.
export function attachBlocks(card, path) {
  let filled = false;
  const load = async () => {
    try {
      const [{ blocks }, name] = await Promise.all([api.get(path), api.poolNames()]);
      bind(card, { blocks, none: blocks.length === 0 }, { pool: name });
      filled = true;
    } catch (e) {
      if (!filled) bind(card, { failed: true });
    }
  };
  document.addEventListener("puntohunter:tip", load);
  load();
}
