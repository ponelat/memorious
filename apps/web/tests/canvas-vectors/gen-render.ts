/** Regenerate with bun tests/canvas-vectors/gen-render.ts from apps/web. */
import {InkBuffers} from '../../src/canvas/ink'
const p = [[4,8,.5,0],[14,10,.9,.2],[28,6,.3,0]]
const pencil = {tool:'pencil',seed:7,size:6,op:.7,pts:p}
const pen = {tool:'pen',seed:1,size:4,op:.6,pts:[[4,17,.5,0],[28,17,.7,0]]}
const cases = [[pencil],[pen],[pen,pen],[pencil,pen,{tool:'eraser',seed:1,size:4,op:.4,only:['pencil'],pts:[[16,0,.5,0],[16,24,.5,0]]}],[pencil,pen,{tool:'eraser',seed:1,size:4,op:1,pts:[[16,0,.5,0],[16,24,.5,0]]}]]
const result = cases.map(strokes=> { const b = new InkBuffers(); b.resize(32,24,1); b.clear({x:0,y:0,z:1}); for (const s of strokes) b.stampStroke(s as any); return {strokes,pencil:Array.from(b.covPencil),pen:Array.from(b.covPen),opacity:Array.from(b.covPenO)} })
await Bun.write(new URL('./render.json', import.meta.url).pathname,JSON.stringify(result))
