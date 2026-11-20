-- Runtime-generated chunk with a jump beyond 16-bit range: exercises JUMPX.
local lines = { "local t = 0", "if ... then" }
for _ = 1, 40000 do
    lines[#lines + 1] = "    t = t + 1"
end
lines[#lines + 1] = "end"
lines[#lines + 1] = "return t"
local chunk = assert(loadstring(table.concat(lines, "\n")))
print(chunk(true), chunk(false))
