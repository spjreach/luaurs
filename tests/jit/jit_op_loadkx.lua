-- Runtime-generated chunk with more than 32767 constants: exercises LOADKX.
local lines = { "local t = {}" }
for i = 1, 33000 do
    lines[#lines + 1] = "t[" .. i .. "] = \"s" .. i .. "\""
end
lines[#lines + 1] = "print(t[33000], t[1], #t)"
local chunk = assert(loadstring(table.concat(lines, "\n")))
chunk()
