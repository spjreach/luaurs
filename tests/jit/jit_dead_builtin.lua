-- builtins whose result is discarded still check their arguments
local function hot(n, x)
  for i = 1, n do
    vector.create(i, 2, 3)
    vector.create(x, 2)
  end
  return n
end

print(hot(10000, 1))
print(pcall(hot, 10, "oops"))
print(pcall(hot, 10, nil))

-- the discarded table constructor still runs its element expressions
local count = 0
local function side() count = count + 1; return count end
local function ctor(n)
  for _ = 1, n do
    local t = { side(), side() }
    local u = { a = 1, b = 2 }
  end
end
ctor(1000)
print(count)

-- vector arithmetic with operands loaded before the loop
local function accum(vs)
  local acc = vector.create(0, 0, 0)
  for i = 1, #vs do
    acc -= vs[i] * 2
    acc += vs[i]
  end
  return acc
end
local vs = {}
for i = 1, 100 do vs[i] = vector.create(i, i, i) end
for _ = 1, 50 do accum(vs) end
print(accum(vs))
print(pcall(accum, { 1, 2, 3 }))
