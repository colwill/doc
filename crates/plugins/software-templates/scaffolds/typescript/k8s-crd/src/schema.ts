// Prints what the zod schema comes to, so it can be compared with the CRD in config/crd/bases
// rather than trusted: `npm run schema`.

import { Resource } from "./types.js";

console.log(JSON.stringify(Resource.shape, null, 2));
