-- deopt/bail paths: a type guess goes stale and the loop has to leave native code
-- without losing register state.  The same local changes type between iterations.
local function show(...) print(...) end

-- one register: number -> string -> table -> nil
local function churn(n)
  local out = {}
  for i = 1, n do
    local x = i * 2
    if i % 3 == 0 then x = "s" .. i end
    if i % 5 == 0 then x = {i} end
    if i % 7 == 0 then x = nil end
    out[#out + 1] = type(x) .. ":" .. tostring(x)
  end
  return table.concat(out, "|")
end
for _ = 1, 4 do churn(80) end
show(churn(30))

-- the arithmetic site fails after the loop has specialized for numbers
local function arithmetic(n)
  local out = {}
  local acc = 0
  for i = 1, n do
    local x = i
    if i % 4 == 0 then x = "x" .. i end
    if i % 4 == 0 then
      local ok, err = pcall(function() return x + 1 end)
      local msg = tostring(err):gsub("^.-:%d+: ", "")
      out[#out + 1] = tostring(ok) .. ":" .. msg
    else
      acc += x * 2
    end
  end
  return acc, table.concat(out, "|")
end
for _ = 1, 3 do arithmetic(40) end
show(arithmetic(12))

-- the register is a table first and a number later
local function table_then_num(n)
  local acc = 0
  for i = 1, n do
    local v = {i, i + 1}
    acc += v[1] + v[2]
    v = i
    acc += v
  end
  return acc
end
for _ = 1, 3 do table_then_num(50) end
show(table_then_num(20))

-- plus changes meaning: number, vector, metatable
local mt = {__add = function(a, b) return a.v + b.v end}
local function poly(a, b) return a + b end
for _ = 1, 3 do poly(1, 2); poly(vector.create(1, 2, 3), vector.create(3, 2, 1)) end
show(poly(1, 2), poly(vector.create(1, 2, 3), vector.create(3, 2, 1)), poly(setmetatable({v = 10}, mt), setmetatable({v = 5}, mt)))
show(pcall(poly, setmetatable({v = 10}, mt), 1))

-- loop-carried register with continue/break and a type change
local function carry(n)
  local s, x = 0, 1
  for i = 1, n do
    x = (i % 3 == 0) and ("s" .. i) or i
    if i % 5 == 0 then continue end
    if type(x) == "number" then s += x else s += #x end
    if s > 100 then break end
  end
  return s, x
end
for _ = 1, 3 do carry(100) end
show(carry(50))

-- an upvalue changes type between calls
local shared = 1
local function bump() shared = (type(shared) == "number") and ("v" .. shared) or 1 end
local function use(n)
  local total = 0
  for i = 1, n do
    if type(shared) == "number" then total += shared else total += #shared end
    bump()
  end
  return total, shared
end
for _ = 1, 3 do use(30) end
show(use(10))

-- a table field changes type under a loop that caches its element type
local function index_poly(t, k, n)
  local acc = 0
  for i = 1, n do
    local v = t[k]
    if type(v) == "number" then acc += v else acc += #tostring(v) end
    t[k] = (i % 2 == 0) and i or ("s" .. i)
  end
  return acc
end
local t = {x = 1}
for _ = 1, 3 do index_poly(t, "x", 40) end
show(index_poly(t, "x", 10), t.x)

-- errors raised from native code still unwind to pcall with the original value
local function boom(n)
  local acc = 0
  for i = 1, n do
    acc += i
    if i == 7 then error({code = i}) end
  end
  return acc
end
for _ = 1, 3 do pcall(boom, 100) end
local ok, err = pcall(boom, 10)
show(ok, type(err), err.code)
