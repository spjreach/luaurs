-- GC objects stored into old tables while the collector runs incrementally
local keep = {}
for i = 1, 64 do keep[i] = 0 end
local holder = {a = 0, b = 0}
local strs = {"x", "y", "z", "w"}
local function fill(n)
  for i = 1, n do
    local t = {re = i, im = -i}          -- DUPTABLE + field stores
    keep[(i % 64) + 1] = t               -- array store of a new table (barrier)
    holder.a = t                         -- field store of a new table (barrier)
    holder.b = strs[(i % 4) + 1]         -- field store of a string
  end
end
for r = 1, 30 do
  fill(20000)
  local junk = {} for j = 1, 2000 do junk[j] = {j} end
end
collectgarbage()
local s = 0
for i = 1, 64 do s = s + keep[i].re + keep[i].im * 2 end
print(s, holder.a.re, holder.b)
-- length
local function lens(t, n)
  local s = 0
  for i = 1, n do s = s + #t + #"hello" end
  return s
end
print(lens({1, 2, 3}, 1000), lens({}, 1000))
-- NEWTABLE in a loop with array writes
local function mk(n)
  local last
  for i = 1, n do local t = {} ; last = t end
  return last ~= nil
end
print(mk(100000))
-- many allocations: memory must stay bounded
local before = collectgarbage("count")
local function churn(n) local x for i = 1, n do x = {k = i} end return x.k end
print(churn(3000000))
print(collectgarbage("count") - before < 20000)
