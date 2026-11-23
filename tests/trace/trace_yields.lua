local co = coroutine.wrap(function()
  local s = 0
  for i = 1, 3000 do
    local ok, v = pcall(function() if i % 7 == 0 then return coroutine.yield(i) end return i end)
    s = s + v
  end
  return "sum " .. s
end)
local acc = 0
for k = 1, 500 do local r = co(k) if type(r) == "number" then acc = acc + r else print(r) break end end
print(acc)
