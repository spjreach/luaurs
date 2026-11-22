local function A(i, j)
  local ij = i + j - 1
  return 1.0 / (ij * (ij - 1) * 0.5 + i)
end
local function run(n)
  local s = 0
  for i = 1, n do s = s + A(i, 3) end
  return s
end
for k = 1, 5 do print(run(100000)) end
-- upvalue (mutable) + math
local scale = 2
local function g(x) return math.sqrt(x) * scale + math.floor(x / 3) + math.abs(-x) end
local function run2(n)
  local s = 0
  for i = 1, n do s = s + g(i) end
  return s
end
print(run2(100000)); scale = 3; print(run2(100000))
-- exit inside the inlined callee (type change on the 2nd call)
local function h(t, i) if t[i] > 5 then return t[i] * 2 end return t[i] end
local function run3(t)
  local s = 0
  for i = 1, #t do s = s + h(t, i) end
  return s
end
local t = {} for i = 1, 1000 do t[i] = i % 10 end
print(run3(t))
t[500] = 0.5; print(run3(t))
local u = {} for i = 1, 1000 do u[i] = i % 10 end
print(run3(u))
-- nested inline, missing params
local function k1(a, b) return (b or 0) + a end
local function k2(a) return k1(a) * 2 end
local s = 0 for i = 1, 100000 do s = s + k2(i) end print(s)
-- callee changes
local fns = {function(x) return x + 1 end, function(x) return x * 2 end}
local function run4(f) local s = 0 for i = 1, 1000 do s = s + f(i) end return s end
print(run4(fns[1]), run4(fns[2]), run4(fns[1]))
