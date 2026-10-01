import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
const { workRows } = createRequire(import.meta.url)("./work.js");
test("Work rows filter source and state and keep assigned pane", () => {
 const jobs=[{ item:{goal:"Implement tests",source:"github:40"},message:{jobState:"IN_PROGRESS",jobId:"job"},assignment:{pane:"tron~%1",name:"intake"}}, { item:{goal:"Review",source:"slack"},message:{jobState:"PENDING"}}];
 assert.equal(workRows(jobs,"github:40","IN_PROGRESS")[0].pane,"tron~%1");
 assert.equal(workRows(jobs,"slack","PENDING").length,1);
 assert.deepEqual(workRows(null),[]);
 assert.equal(workRows(Array(600).fill(jobs[0])).length,500);
});
