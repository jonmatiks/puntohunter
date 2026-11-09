// The blocks page, blocks.html: the pool's blocks, read again when the chain tip moves.

import { attachBlocks } from "../blocks.js";
import "../common.js";

attachBlocks(document.getElementById("blocks"), "/blocks");
