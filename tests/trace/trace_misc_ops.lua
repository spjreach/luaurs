local function nots(n)
  local c, flag = 0, false
  for i = 1, n do flag = not flag; if not flag then c = c + 1 end; if not nil then c = c + 1 end end
  return c
end
print(nots(100000))
local function pows(n) local s = 0 for i = 1, n do s = s + (i % 7) ^ 1.5 + 2 ^ (i % 5) end return s end
print(pows(100000))
local function cats(n)
  local last, len = "", 0
  for i = 1, n do last = "k" .. i .. ":" .. (i % 3); len = len + #last end
  return last, len
end
print(cats(50000))
print(pcall(function() local s = "" for i = 1, 100 do s = "a" .. (i == 90 and {} or i) end end))
-- upvalue stores: closed (counter shared by closures) and open on the frame
local count = 0
local function bump() count = count + 1 end
local function run(n) for i = 1, n do bump() end return count end
print(run(100000))
local function opened(n)
  local acc = 0
  local function add(x) acc = acc + x end
  for i = 1, n do add(i) end
  return acc
end
print(opened(100000))
local function strs(n)
  local total = 0
  local function emit(s) total = total + #s end
  for i = 1, n do emit("v" .. i) end
  return total
end
print(strs(50000))
