-- s:byte(i) inline, s:sub(i, j) direct (with a per-site cache)
local strs = { "hello world", "", "x", "abcdefghijklmnopqrstuvwxyz", "\0\1\255z" }
local idx = { 1, 2, -1, -3, 0, 5, 30, -40, 2.7, -2.2, 1e10, -1e10 }
local function bytes(n)
  local s, nils = 0, 0
  for r = 1, n do
    local str = strs[(r % #strs) + 1]
    local i = idx[(r % #idx) + 1]
    local b = str:byte(i)
    if b then s = s + b else nils = nils + 1 end
    s = s + (str:byte() or 0) + (string.byte(str, 2) or 0)
  end
  return s, nils
end
print(bytes(6000))
local function subs(n)
  local len, last, h = 0, "", 0
  for r = 1, n do
    local str = strs[(r % #strs) + 1]
    local i = idx[(r % #idx) + 1]
    local j = idx[((r * 7) % #idx) + 1]
    local a = str:sub(i, j)
    local b = str:sub(i)
    local c = string.sub(str, 2, -2)
    len = len + #a + #b + #c
    last = a .. "|" .. b .. "|" .. c
    if r % 1000 == 0 then collectgarbage() end
    for k = 1, #a do h = (h * 31 + a:byte(k)) % 1000003 end
  end
  return len, last, h
end
print(subs(6000))
-- substrings kept and compared after collections
local keep = {}
local function k(n)
  for r = 1, n do
    local s = ("item" .. (r % 37)):sub(2, -1)
    if r % 50 == 0 then keep[#keep + 1] = s collectgarbage() end
  end
  local same = 0
  for i = 2, #keep do if keep[i] == ("item" .. ((i * 50) % 37)):sub(2) then same = same + 1 end end
  return #keep, same
end
print(k(5000))
