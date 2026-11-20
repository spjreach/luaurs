-- for loops whose key is unused, or only written, inside the body
local t = {}
for i = 1, 50 do t[i] = i * 2 end
t.x = "hash"
local function f(t)
  local s = 0
  for _, v in t do
    if type(v) == "number" then s += v end
  end
  return s
end
local function g(t)
  local out = 0
  for k, v in ipairs(t) do
    k = v / 2        -- key overwritten before any read
    out += k
  end
  return out
end
local function h(t)
  local n = 0
  for k in t do n += 1 end
  local last
  for k, v in t do last = k end
  return n, last
end
local function m(t)
  local acc = {}
  for _, v in t do
    _ = 3            -- written as a number, then read
    acc[#acc + 1] = _ + (type(v) == "number" and v or 0)
  end
  return #acc, acc[1], acc[#acc]
end
for _ = 1, 3 do f(t) g(t) h(t) m(t) end
print(f(t), g(t), h(t))
print(m(t))
